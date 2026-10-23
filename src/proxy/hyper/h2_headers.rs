use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::{Instant, Sleep},
};

pub(super) struct HeaderIo<T> {
    pub inner: T,
    preface: Option<usize>,
    disabled: bool,
    frame: [u8; 9],
    header_bytes: usize,
    remaining: usize,
    block: bool,
    timer: Option<Pin<Box<Sleep>>>,
    started: Option<Instant>,
    timeout: Duration,
    failure: Arc<AtomicU8>,
}
impl<T> HeaderIo<T> {
    pub fn new(inner: T, timeout: Duration, failure: Arc<AtomicU8>) -> Self {
        Self {
            inner,
            preface: Some(0),
            disabled: false,
            frame: [0; 9],
            header_bytes: 0,
            remaining: 0,
            block: false,
            timer: None,
            started: None,
            timeout,
            failure,
        }
    }
    fn start(&mut self) {
        if self.started.is_none() {
            self.started = Some(Instant::now());
        }
    }
    fn clear(&mut self) {
        self.started = None;
        self.timer = None;
    }
    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        // Complete frames read in one batch need no timer allocation.
        if self.timer.is_none()
            && let Some(started) = self.started
        {
            self.timer = Some(Box::pin(tokio::time::sleep_until(started + self.timeout)));
        }
        if self
            .timer
            .as_mut()
            .is_some_and(|t| t.as_mut().poll(cx).is_ready())
        {
            self.failure.store(1, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP/2 header block timed out",
            ));
        }
        Ok(())
    }
    fn observe(&mut self, mut bytes: &[u8]) {
        const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        if self.disabled {
            return;
        }
        while !bytes.is_empty() {
            if let Some(n) = self.preface {
                if bytes[0] != PREFACE[n] {
                    self.disabled = true;
                    self.clear();
                    return;
                }
                self.start();
                self.preface = (n + 1 < PREFACE.len()).then_some(n + 1);
                bytes = &bytes[1..];
                if self.preface.is_none() {
                    self.clear();
                }
                continue;
            }
            if self.header_bytes < 9 {
                self.start();
                let n = (9 - self.header_bytes).min(bytes.len());
                self.frame[self.header_bytes..self.header_bytes + n].copy_from_slice(&bytes[..n]);
                self.header_bytes += n;
                bytes = &bytes[n..];
                if self.header_bytes != 9 {
                    continue;
                }
                self.remaining = ((self.frame[0] as usize) << 16)
                    | ((self.frame[1] as usize) << 8)
                    | self.frame[2] as usize;
                if matches!(self.frame[3], 1 | 5) {
                    self.block = true;
                }
                if !self.block {
                    self.clear();
                }
            }
            let n = self.remaining.min(bytes.len());
            self.remaining -= n;
            bytes = &bytes[n..];
            if self.remaining == 0 {
                // CONTINUATION blocks all streams; byte trickling must not renew
                // its deadline. The HTTP/2 engine retains ownership of validation.
                if matches!(self.frame[3], 1 | 5 | 9) && self.frame[4] & 4 != 0 {
                    self.block = false;
                    self.clear();
                }
                self.header_bytes = 0;
            }
        }
    }
}
impl<T: AsyncRead + Unpin> AsyncRead for HeaderIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.check(cx)?;
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.observe(&buf.filled()[before..]);
            self.check(cx)?;
        }
        result
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for HeaderIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bytes)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
