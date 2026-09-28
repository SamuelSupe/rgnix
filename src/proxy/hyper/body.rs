use super::io::Connection;
use crate::proxy::{Context as RequestContext, Proxy, completion::Completion};
use bytes::Bytes;
use http_body_util::{Either, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

pub(super) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(super) type Payload = Either<Incoming, Full<Bytes>>;

#[derive(Default)]
pub(super) struct Counters {
    pub request: AtomicU64,
    pub response: AtomicU64,
    pub request_error: AtomicU8,
}

pub(super) struct RequestGuard {
    pub proxy: Proxy,
    pub ctx: Option<RequestContext>,
    pub counts: Arc<Counters>,
    pub version: http::Version,
    pub error: Option<Box<pingora::Error>>,
    pub keepalive: Duration,
    pub io_failure: Arc<AtomicU8>,
}
impl RequestGuard {
    pub fn finish(&mut self, delivered: bool) {
        let Some(mut ctx) = self.ctx.take() else {
            return;
        };
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
            &mut ctx,
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
    inner: Incoming,
    counts: Arc<Counters>,
    max: u64,
}
impl RequestBody {
    pub fn new(inner: Incoming, counts: Arc<Counters>, max: u64) -> Self {
        Self { inner, counts, max }
    }
}
impl Body for RequestBody {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let frame = std::task::ready!(Pin::new(&mut self.inner).poll_frame(cx));
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
                self.counts.request_error.store(2, Ordering::Relaxed);
                Poll::Ready(Some(Err(error.into())))
            }
            None => Poll::Ready(None),
        }
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

pub(super) struct ResponseBody {
    inner: Payload,
    guard: Option<RequestGuard>,
    connection: Arc<Connection>,
    done: bool,
}
impl ResponseBody {
    pub fn new(inner: Payload, guard: RequestGuard, connection: Arc<Connection>) -> Self {
        let done = inner.is_end_stream();
        Self {
            inner,
            guard: Some(guard),
            connection,
            done,
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
        let frame = std::task::ready!(Pin::new(&mut self.inner).poll_frame(cx));
        if let Some(guard) = &mut self.guard {
            match &frame {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        guard
                            .counts
                            .response
                            .fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                }
                Some(Err(error)) => {
                    guard.error = Some(super::request::upstream_error(
                        error.as_ref(),
                        super::request::FailurePhase::Response,
                    ));
                }
                None => {}
            }
        }
        self.done = frame.is_none() || self.inner.is_end_stream();
        Poll::Ready(frame)
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}
impl Drop for ResponseBody {
    fn drop(&mut self) {
        // A discarded body owns the permits until here and records cancellation once.
        if let Some(guard) = self.guard.take()
            && self.done
        {
            self.connection.queue(guard);
        }
    }
}
