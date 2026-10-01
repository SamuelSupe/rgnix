use super::io::Connection;
use crate::proxy::{Context as RequestContext, Proxy, completion::Completion};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

pub(super) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(super) enum Payload {
    Incoming(Incoming),
    Full(Full<Bytes>),
    Stream(Pin<Box<dyn Body<Data = Bytes, Error = BoxError> + Send>>),
    #[cfg(target_os = "linux")]
    SendFile(hyper::ext::SendFile),
}
impl Body for Payload {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match &mut *self {
            #[cfg(target_os = "linux")]
            Self::SendFile(_) => Poll::Ready(None),
            Self::Incoming(body) => Pin::new(body)
                .poll_frame(cx)
                .map(|f| f.map(|r| r.map_err(Into::into))),
            Self::Full(body) => Pin::new(body)
                .poll_frame(cx)
                .map(|f| f.map(|r| r.map_err(|never| match never {}))),
            Self::Stream(body) => body.as_mut().poll_frame(cx),
        }
    }
    fn size_hint(&self) -> SizeHint {
        match self {
            #[cfg(target_os = "linux")]
            Self::SendFile(_) => SizeHint::with_exact(0),
            Self::Incoming(b) => b.size_hint(),
            Self::Full(b) => b.size_hint(),
            Self::Stream(b) => b.size_hint(),
        }
    }
    fn is_end_stream(&self) -> bool {
        match self {
            #[cfg(target_os = "linux")]
            Self::SendFile(_) => true,
            Self::Incoming(b) => b.is_end_stream(),
            Self::Full(b) => b.is_end_stream(),
            Self::Stream(b) => b.is_end_stream(),
        }
    }
}

#[derive(Default)]
pub(super) struct Counters {
    pub request: AtomicU64,
    pub response: AtomicU64,
    pub request_error: AtomicU8,
}

pub(super) struct RequestGuard {
    pub proxy: Proxy,
    pub ctx: RequestContext,
    pub finished: bool,
    pub counts: Arc<Counters>,
    pub version: http::Version,
    pub error: Option<Box<pingora::Error>>,
    pub keepalive: Duration,
    pub io_failure: Arc<AtomicU8>,
    pub connection: Weak<Connection>,
    pub deadline: Option<std::time::Instant>,
    pub upgrade: Option<super::tunnel::Tunnel>,
}
impl RequestGuard {
    pub fn finish(&mut self, delivered: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        if matches!(self.version, http::Version::HTTP_2 | http::Version::HTTP_3)
            && let Some(connection) = self.connection.upgrade()
        {
            connection.finish_stream(self.keepalive);
        }
        let ctx = &mut self.ctx;
        if !delivered && self.error.is_none() {
            let kind = match self.io_failure.load(Ordering::Relaxed) {
                1 => pingora::ErrorType::ReadTimedout,
                2 => pingora::ErrorType::WriteTimedout,
                _ => pingora::ErrorType::ConnectionClosed,
            };
            self.error = Some(
                pingora::Error::explain(kind, "downstream did not complete response").into_down(),
            );
        }
        let status = if ctx.status != 0 { ctx.status } else { 499 };
        self.proxy.complete(
            ctx,
            Completion {
                version: self.version,
                status,
                request_bytes: self.counts.request.load(Ordering::Relaxed) as usize,
                response_bytes: self.counts.response.load(Ordering::Relaxed) as usize,
                error: self.error.as_deref(),
            },
        );
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.finish(false);
    }
}

pub(super) struct RequestBody {
    stream: Option<Box<Upload>>,
}
impl RequestBody {
    pub fn new(inner: Payload, counts: Arc<Counters>, max: u64, timeout: Duration) -> Self {
        // Keeping upload state inline enlarges every client request and its
        // channel/future moves, including the common requests without a body.
        Self {
            stream: (!inner.is_end_stream())
                .then(|| Box::new(Upload::new(inner, counts, max, timeout))),
        }
    }

    pub async fn inspect(
        &mut self,
        policy: crate::body::BodyPolicy,
    ) -> pingora::Result<Option<crate::body::BodyView>> {
        match &mut self.stream {
            Some(stream) => stream.inspect(policy).await,
            None => Ok(
                (policy.inspection != crate::body::Inspection::Off).then(|| {
                    crate::body::BodyView {
                        bytes: Bytes::new(),
                        complete: true,
                    }
                }),
            ),
        }
    }

    pub async fn reject_upgrade_body(&mut self) -> pingora::Result<()> {
        match &mut self.stream {
            Some(stream) => stream.reject_upgrade_body().await,
            None => Err(super::super::error(
                400,
                "WebSocket handshake must not contain a body",
            )),
        }
    }

    pub fn set_mirror(&mut self, mut mirror: Option<Box<crate::proxy::mirror::MirrorRequest>>) {
        if let Some(stream) = &mut self.stream {
            stream.mirror = mirror;
        } else {
            crate::proxy::mirror::feed(&mut mirror, None, true);
        }
    }
}
impl Body for RequestBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match &mut self.stream {
            Some(stream) => Pin::new(stream.as_mut()).poll_frame(cx),
            None => Poll::Ready(None),
        }
    }

    fn size_hint(&self) -> SizeHint {
        self.stream
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), |stream| stream.size_hint())
    }

    fn is_end_stream(&self) -> bool {
        self.stream
            .as_ref()
            .is_none_or(|stream| stream.is_end_stream())
    }
}

struct Upload {
    mirror: Option<Box<crate::proxy::mirror::MirrorRequest>>,
    inner: Payload,
    counts: Arc<Counters>,
    max: u64,
    replay: VecDeque<Frame<Bytes>>,
    read: super::io::Deadline,
    timeout: Duration,
}
impl Upload {
    fn new(inner: Payload, counts: Arc<Counters>, max: u64, timeout: Duration) -> Self {
        Self {
            inner,
            counts,
            max,
            replay: VecDeque::new(),
            mirror: None,
            read: Default::default(),
            timeout,
        }
    }
    pub async fn inspect(
        &mut self,
        policy: crate::body::BodyPolicy,
    ) -> pingora::Result<Option<crate::body::BodyView>> {
        use crate::body::Inspection;
        let (limit, full) = match policy.inspection {
            Inspection::Off => return Ok(None),
            Inspection::Full(limit) => (limit, true),
            Inspection::Prefix(limit) => (limit, false),
        };
        let read = async {
            let mut visible = bytes::BytesMut::new();
            let mut total = 0usize;
            let mut complete = self.inner.is_end_stream();
            while !complete && (full || total < limit) {
                let frame = futures::future::poll_fn(|cx| self.poll_inner(cx)).await;
                let Some(frame) = frame else {
                    complete = true;
                    break;
                };
                let frame =
                    frame.map_err(
                        |_| match self.counts.request_error.load(Ordering::Relaxed) {
                            1 => super::super::error(413, "request body too large"),
                            3 => super::super::error(408, "request body timed out"),
                            _ => super::super::error(400, "incomplete request body"),
                        },
                    )?;
                if let Some(data) = frame.data_ref() {
                    total = total.saturating_add(data.len());
                    if full && total > limit {
                        return Err(super::super::error(
                            413,
                            "request body exceeds full inspection limit",
                        ));
                    }
                    visible.extend_from_slice(&data[..data.len().min(limit - visible.len())]);
                }
                self.replay.push_back(frame);
                complete = self.inner.is_end_stream();
            }
            Ok(Some(crate::body::BodyView {
                bytes: visible.freeze(),
                complete: complete && total <= limit,
            }))
        };
        tokio::time::timeout(policy.timeout, read)
            .await
            .map_err(|_| super::super::error(408, "request body inspection timed out"))?
    }

    pub async fn reject_upgrade_body(&mut self) -> pingora::Result<()> {
        while let Some(frame) = futures::future::poll_fn(|cx| self.poll_inner(cx)).await {
            frame.map_err(
                |_| match self.counts.request_error.load(Ordering::Relaxed) {
                    1 => super::super::error(413, "request body too large"),
                    3 => super::super::error(408, "request body timed out"),
                    _ => super::super::error(400, "incomplete upgrade request body"),
                },
            )?;
        }
        Err(super::super::error(
            400,
            "WebSocket handshake must not contain a body",
        ))
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let frame = match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Pending => {
                if let Poll::Ready(Err(error)) = self.read.check(cx, self.timeout) {
                    self.counts.request_error.store(3, Ordering::Relaxed);
                    return Poll::Ready(Some(Err(error.into())));
                }
                return Poll::Pending;
            }
            Poll::Ready(frame) => {
                self.read.clear();
                frame
            }
        };
        match frame {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    let size = self
                        .counts
                        .request
                        .fetch_add(data.len() as u64, Ordering::Relaxed)
                        + data.len() as u64;
                    if self.max > 0 && size > self.max {
                        self.counts.request_error.store(1, Ordering::Relaxed);
                        return Poll::Ready(Some(Err(
                            "request body exceeds configured limit".into()
                        )));
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Some(Err(error)) => {
                // Transport cancellation can follow a size/idle error. Preserve
                // the originating failure instead of reporting its cleanup EOF.
                let _ = self.counts.request_error.compare_exchange(
                    0,
                    2,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                Poll::Ready(Some(Err(error)))
            }
            None => Poll::Ready(None),
        }
    }
}
impl Body for Upload {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let frame = match self.replay.pop_front() {
            Some(frame) => Some(Ok(frame)),
            None => std::task::ready!(self.poll_inner(cx)),
        };
        match &frame {
            Some(Ok(frame)) => {
                let end = frame.is_trailers() || self.is_end_stream();
                crate::proxy::mirror::feed(&mut self.mirror, frame.data_ref(), end);
            }
            None => crate::proxy::mirror::feed(&mut self.mirror, None, true),
            Some(Err(_)) => {
                self.mirror = None;
            }
        }
        Poll::Ready(frame)
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = self.inner.size_hint();
        let replay: u64 = self
            .replay
            .iter()
            .filter_map(|f| f.data_ref())
            .map(|b| b.len() as u64)
            .sum();
        if let Some(exact) = hint.exact() {
            hint.set_exact(exact.saturating_add(replay));
        } else {
            hint.set_lower(hint.lower().saturating_add(replay));
        }
        hint
    }
    fn is_end_stream(&self) -> bool {
        self.replay.is_empty() && self.inner.is_end_stream()
    }
}

pub(super) struct ResponseBody {
    inner: Payload,
    // The context stays at one address across body polling and socket completion.
    guard: Option<ResponseGuard>,
    connection: Arc<Connection>,
    done: bool,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    read: super::io::Deadline,
    timeout: Duration,
}

pub(super) enum ResponseGuard {
    Full(Box<RequestGuard>),
    Plain(Box<super::plain::Guard>),
}
impl ResponseGuard {
    pub fn finish(&mut self, delivered: bool) {
        match self {
            Self::Full(guard) => guard.finish(delivered),
            Self::Plain(guard) => guard.finish(delivered),
        }
    }
    pub fn version(&self) -> http::Version {
        match self {
            Self::Full(guard) => guard.version,
            Self::Plain(_) => http::Version::HTTP_11,
        }
    }
    pub fn keepalive(&self) -> Duration {
        match self {
            Self::Full(guard) => guard.keepalive,
            Self::Plain(guard) => guard.route.settings.keepalive,
        }
    }
    fn set_error(&mut self, error: Box<pingora::Error>) {
        match self {
            Self::Full(guard) => guard.error = Some(error),
            Self::Plain(guard) => guard.error = Some(error),
        }
    }
    pub(super) fn counts(&self) -> &Counters {
        match self {
            Self::Full(guard) => &guard.counts,
            Self::Plain(guard) => &guard.counts,
        }
    }
}
impl ResponseBody {
    #[cfg(feature = "http3")]
    pub fn settings(&self) -> Option<&crate::model::Settings> {
        match self.guard.as_ref()? {
            ResponseGuard::Full(g) => g.ctx.route.as_ref().map(|r| &r.settings),
            ResponseGuard::Plain(g) => Some(&g.route.settings),
        }
    }
    #[cfg(feature = "http3")]
    pub fn delivered(&mut self) {
        if let Some(mut guard) = self.guard.take() {
            guard.finish(true);
        }
    }
    pub fn new(inner: Payload, guard: Box<RequestGuard>, connection: Arc<Connection>) -> Self {
        let done = inner.is_end_stream();
        let deadline = guard
            .deadline
            .into_iter()
            .chain(
                guard
                    .ctx
                    .route
                    .as_ref()
                    .and_then(|r| r.settings.gateway.as_ref())
                    .and_then(|p| p.timeouts.backend_request)
                    .and_then(|d| guard.ctx.upstream_started?.checked_add(d)),
            )
            .min()
            .map(|until| Box::pin(tokio::time::sleep_until(until.into())));
        let timeout = guard
            .ctx
            .route
            .as_ref()
            .map_or(Duration::from_secs(60), |r| r.settings.read_timeout);
        Self {
            inner,
            guard: Some(ResponseGuard::Full(guard)),
            connection,
            done,
            deadline,
            read: Default::default(),
            timeout,
        }
    }
    pub fn plain(
        inner: Payload,
        guard: Box<super::plain::Guard>,
        connection: Arc<Connection>,
    ) -> Self {
        let timeout = guard.route.settings.read_timeout;
        Self {
            done: inner.is_end_stream(),
            inner,
            guard: Some(ResponseGuard::Plain(guard)),
            connection,
            deadline: None,
            read: Default::default(),
            timeout,
        }
    }
    pub fn untracked(inner: Payload, connection: Arc<Connection>) -> Self {
        Self {
            done: inner.is_end_stream(),
            inner,
            guard: None,
            connection,
            deadline: None,
            read: Default::default(),
            timeout: Duration::from_secs(60),
        }
    }
}
impl Body for ResponseBody {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        use std::future::Future;
        if let Some(deadline) = &mut self.deadline
            && deadline.as_mut().poll(cx).is_ready()
        {
            if let Some(guard) = &mut self.guard {
                guard.set_error(super::super::error(504, "request deadline exceeded"));
            }
            return Poll::Ready(Some(Err("request deadline exceeded".into())));
        }
        let frame = match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Pending => {
                let timeout = self.timeout;
                if let Poll::Ready(Err(error)) = self.read.check(cx, timeout) {
                    if let Some(guard) = &mut self.guard {
                        guard.set_error(
                            pingora::Error::explain(
                                pingora::ErrorType::ReadTimedout,
                                "upstream body timed out",
                            )
                            .into_up(),
                        );
                    }
                    return Poll::Ready(Some(Err(error.into())));
                }
                return Poll::Pending;
            }
            Poll::Ready(frame) => {
                self.read.clear();
                frame
            }
        };
        if let Some(guard) = &mut self.guard {
            match &frame {
                Some(Ok(frame)) => {
                    if let Some(value) = frame.trailers_ref().and_then(|t| t.get("grpc-status"))
                        && let ResponseGuard::Full(guard) = guard
                    {
                        guard.ctx.grpc_status = Some(super::request::grpc_status(value));
                    }
                    if let Some(data) = frame.data_ref() {
                        guard
                            .counts()
                            .response
                            .fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                }
                Some(Err(error)) => {
                    guard.set_error(super::request::upstream_error(
                        error.as_ref(),
                        super::request::FailurePhase::Response,
                    ));
                }
                None => {}
            }
        }
        self.done = frame.is_none()
            || frame
                .as_ref()
                .is_some_and(|f| f.as_ref().is_ok_and(Frame::is_trailers))
            || self.inner.is_end_stream();
        Poll::Ready(frame)
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.done
    }
}
impl Drop for ResponseBody {
    fn drop(&mut self) {
        // A discarded body owns the permits until here and records cancellation once.
        if let Some(guard) = self.guard.take()
            && self.done
            && guard.version() != http::Version::HTTP_3
        {
            self.connection.queue(guard);
        }
    }
}
