use super::body::RequestGuard;
use std::{
    collections::VecDeque,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    time::{Instant, Sleep},
};

pub(super) struct Connection {
    pending: Mutex<VecDeque<RequestGuard>>,
    pub failure: Arc<AtomicU8>,
    pub read_timeout_ms: AtomicU64,
}
impl Default for Connection {
    fn default() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            failure: Arc::new(AtomicU8::new(0)),
            read_timeout_ms: AtomicU64::new(60_000),
        }
    }
}
impl Connection {
    pub fn queue(&self, request: RequestGuard) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
    }
    fn flushed(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(mut request) = pending.pop_front() {
            self.read_timeout_ms.store(
                request.keepalive.as_millis().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
            request.finish(true);
        }
    }
    fn closed(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

#[derive(Default)]
struct Deadline {
    timer: Option<Pin<Box<Sleep>>>,
    active: bool,
    duration: Duration,
}
impl Deadline {
    fn check(&mut self, cx: &mut Context<'_>, duration: Duration) -> Poll<io::Result<()>> {
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
    stream: TcpStream,
    state: Arc<Connection>,
    read: Deadline,
    write: Deadline,
}
impl TrackedIo {
    pub fn new(stream: TcpStream, state: Arc<Connection>) -> Self {
        Self {
            stream,
            state,
            read: Deadline::default(),
            write: Deadline::default(),
        }
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
            Poll::Pending => match self.write.check(cx, Duration::from_secs(60)) {
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
            Poll::Pending => match self.write.check(cx, Duration::from_secs(60)) {
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
