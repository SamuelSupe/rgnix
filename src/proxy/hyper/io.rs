use super::body::ResponseGuard;
use std::{
    collections::VecDeque,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::{Instant, Sleep},
};

pub(super) struct Connection {
    pub admission: Option<Arc<super::admission::Lease>>,
    pub tls: bool,
    pub proxy_address: Option<std::net::SocketAddr>,
    pub client_certificate: Option<Arc<dyn std::any::Any + Send + Sync>>,
    tunnel: Mutex<Vec<Tunnel>>,
    pending: Mutex<VecDeque<ResponseGuard>>,
    pub failure: Arc<AtomicU8>,
    pub read_timeout_ms: AtomicU64,
    pub write_timeout_ms: AtomicU64,
    pub multiplexed: AtomicBool,
    pub request_active: AtomicBool,
    active_streams: AtomicUsize,
    read_waker: futures::task::AtomicWaker,
}
impl Default for Connection {
    fn default() -> Self {
        Self {
            admission: None,
            tls: false,
            proxy_address: None,
            client_certificate: None,
            tunnel: Mutex::new(Vec::new()),
            pending: Mutex::new(VecDeque::new()),
            failure: Arc::new(AtomicU8::new(0)),
            read_timeout_ms: AtomicU64::new(60_000),
            write_timeout_ms: AtomicU64::new(60_000),
            multiplexed: AtomicBool::new(false),
            request_active: AtomicBool::new(false),
            active_streams: AtomicUsize::new(0),
            read_waker: futures::task::AtomicWaker::new(),
        }
    }
}
impl Connection {
    pub fn start_stream(&self) {
        self.multiplexed.store(true, Ordering::Relaxed);
        self.active_streams.fetch_add(1, Ordering::Relaxed);
        self.read_waker.wake();
    }
    pub fn finish_stream(&self, keepalive: Duration) {
        self.read_timeout_ms.store(
            keepalive.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        self.active_streams.fetch_sub(1, Ordering::Relaxed);
        self.read_waker.wake();
    }
    pub fn set_tunnel(&self, task: tokio::task::JoinHandle<()>) {
        let mut tunnels = self.tunnel.lock().unwrap_or_else(|e| e.into_inner());
        tunnels.retain(|task| !task.0.is_finished());
        tunnels.push(Tunnel(task));
    }
    pub async fn wait_tunnel(&self) {
        let tunnels = std::mem::take(&mut *self.tunnel.lock().unwrap_or_else(|e| e.into_inner()));
        futures::future::join_all(tunnels.into_iter().map(|mut task| async move {
            let _ = (&mut task.0).await;
        }))
        .await;
    }
    pub fn queue(&self, request: ResponseGuard) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
    }
    fn flushed(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(mut request) = pending.pop_front() {
            if request.version() != http::Version::HTTP_2 {
                self.read_timeout_ms.store(
                    request.keepalive().as_millis().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                self.request_active.store(false, Ordering::Relaxed);
                self.read_waker.wake();
            }
            request.finish(true);
        }
    }
    fn closed(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    #[cfg(target_os = "linux")]
    fn file_bytes(&self, length: usize) {
        if let Some(guard) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .back()
        {
            guard
                .counts()
                .response
                .fetch_add(length as u64, Ordering::Relaxed);
        }
    }
}

struct Tunnel(tokio::task::JoinHandle<()>);
impl Drop for Tunnel {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Default)]
pub(super) struct Deadline {
    timer: Option<Pin<Box<Sleep>>>,
    active: bool,
    duration: Duration,
}
impl Deadline {
    pub(super) fn clear(&mut self) {
        self.active = false;
    }

    pub(super) fn restart(&mut self) {
        if self.active {
            self.timer
                .as_mut()
                .unwrap()
                .as_mut()
                .reset(Instant::now() + self.duration);
        }
    }

    pub(super) fn check(
        &mut self,
        cx: &mut Context<'_>,
        duration: Duration,
    ) -> Poll<io::Result<()>> {
        if !self.active || self.duration != duration {
            self.duration = duration;
            let until = Instant::now() + duration;
            if let Some(timer) = &mut self.timer {
                timer.as_mut().reset(until);
            } else {
                self.timer = Some(Box::pin(tokio::time::sleep_until(until)));
            }
            self.active = true;
        }
        if self.timer.as_mut().unwrap().as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(io::ErrorKind::TimedOut.into()))
        } else {
            Poll::Pending
        }
    }
}

pub(super) struct TrackedIo {
    stream: super::h2_headers::HeaderIo<super::tls::Stream>,
    state: Arc<Connection>,
    read: Deadline,
    write: Deadline,
}
impl TrackedIo {
    pub fn new(
        stream: super::tls::Stream,
        state: Arc<Connection>,
        header_timeout: Duration,
    ) -> Self {
        Self {
            stream: super::h2_headers::HeaderIo::new(stream, header_timeout, state.failure.clone()),
            state,
            read: Deadline::default(),
            write: Deadline::default(),
        }
    }
    fn poll_write_deadline(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.state.multiplexed.load(Ordering::Relaxed) {
            self.write.clear();
            return Poll::Pending;
        }
        let timeout = Duration::from_millis(self.state.write_timeout_ms.load(Ordering::Relaxed));
        self.write.check(cx, timeout)
    }
}
impl AsyncRead for TrackedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_read(cx, buf) {
            Poll::Pending => {
                self.state.read_waker.register(cx.waker());
                // A quiet multiplexed connection may still have an active stream.
                // Stream deadlines enforce progress without resetting unrelated RPCs.
                if self.state.request_active.load(Ordering::Relaxed)
                    || (self.state.multiplexed.load(Ordering::Relaxed)
                        && self.state.active_streams.load(Ordering::Relaxed) > 0)
                {
                    self.read.clear();
                    return Poll::Pending;
                }
                let timeout =
                    Duration::from_millis(self.state.read_timeout_ms.load(Ordering::Relaxed));
                let result = self.read.check(cx, timeout);
                if result.is_ready() {
                    self.state.failure.store(1, Ordering::Relaxed);
                }
                result
            }
            ready => {
                self.read.active = false;
                ready
            }
        }
    }
}
impl AsyncWrite for TrackedIo {
    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write_vectored(cx, bufs) {
            Poll::Pending => match self.poll_write_deadline(cx) {
                Poll::Ready(Err(error)) => {
                    self.state.failure.store(2, Ordering::Relaxed);
                    Poll::Ready(Err(error))
                }
                _ => Poll::Pending,
            },
            ready => {
                self.write.active = false;
                ready
            }
        }
    }
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write(cx, buf) {
            Poll::Pending => match self.poll_write_deadline(cx) {
                Poll::Ready(Err(error)) => {
                    self.state.failure.store(2, Ordering::Relaxed);
                    Poll::Ready(Err(error))
                }
                _ => Poll::Pending,
            },
            ready => {
                self.write.active = false;
                ready
            }
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.stream).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.state.flushed();
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
impl Drop for TrackedIo {
    fn drop(&mut self) {
        self.state.closed();
    }
}

impl hyper::rt::Read for TrackedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        hyper::rt::Read::poll_read(
            Pin::new(&mut hyper_util::rt::TokioIo::new(self.get_mut())),
            cx,
            buf,
        )
    }
}
impl hyper::rt::Write for TrackedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(self, cx, buf)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write_vectored(self, cx, buf)
    }
    fn is_write_vectored(&self) -> bool {
        AsyncWrite::is_write_vectored(self)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(self, cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(self, cx)
    }
    #[cfg(target_os = "linux")]
    fn poll_sendfile(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        file: &std::fs::File,
        offset: u64,
        length: usize,
    ) -> Poll<io::Result<usize>> {
        use std::os::fd::AsRawFd;
        let super::tls::Stream::Plain(socket) = &self.stream.inner else {
            return Poll::Ready(Err(io::ErrorKind::Unsupported.into()));
        };
        let ready = socket.poll_write_ready(cx);
        if ready.is_pending() {
            return self.poll_write_deadline(cx).map_ok(|_| 0);
        }
        if let Poll::Ready(Err(error)) = ready {
            return Poll::Ready(Err(error));
        }
        let result = socket.try_io(tokio::io::Interest::WRITABLE, || {
            let mut offset: libc::off_t = offset
                .try_into()
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // The file and socket remain owned for this call; sendfile advances only
            // the local offset, preserving the shared descriptor's seek position.
            let n = unsafe {
                libc::sendfile(socket.as_raw_fd(), file.as_raw_fd(), &mut offset, length)
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match result {
            Ok(n) => {
                self.write.clear();
                self.state.file_bytes(n);
                Poll::Ready(Ok(n))
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let _ = socket.poll_write_ready(cx);
                self.poll_write_deadline(cx).map_ok(|_| 0)
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}
