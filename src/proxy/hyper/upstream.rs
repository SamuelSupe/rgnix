use super::{body::BoxError, io::Deadline};
use hyper::Uri;
use hyper_util::{
    client::legacy::connect::{Connected, Connection, HttpConnector},
    rt::TokioIo,
};
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tower_service::Service;

#[derive(Clone)]
pub(super) struct Connector {
    http: HttpConnector,
    read: Duration,
    write: Duration,
    connect: Duration,
    tls: Option<openssl::ssl::SslConnector>,
    hostname: String,
    http2: bool,
}

impl Connector {
    pub fn new(
        connect: Duration,
        read: Duration,
        write: Duration,
        transport: &crate::upstream::Transport,
        backend: &crate::backend::Backend,
    ) -> anyhow::Result<Self> {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(connect));
        Ok(Self {
            http,
            read,
            write,
            connect,
            tls: backend
                .tls
                .then(|| super::tls::connector(transport))
                .transpose()?,
            hostname: transport
                .server_name
                .clone()
                .unwrap_or_else(|| backend.hostname.clone()),
            http2: transport.protocol == crate::upstream::Protocol::Http2,
        })
    }
}

impl Service<Uri> for Connector {
    type Response = UpstreamIo<super::tls::Stream>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connecting = self.http.call(uri);
        let (read, write, timeout) = (self.read, self.write, self.connect);
        let (tls, hostname, http2) = (self.tls.clone(), self.hostname.clone(), self.http2);
        Box::pin(async move {
            tokio::time::timeout(timeout, async move {
                let stream = connecting.await?.into_inner();
                let stream = match tls {
                    Some(tls) => super::tls::connect(&tls, &hostname, stream, http2).await?,
                    None => super::tls::Stream::Plain(stream),
                };
                let multiplexed = http2 || stream.h2();
                Ok(UpstreamIo::new(stream, read, write, multiplexed))
            })
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
        })
    }
}

pub(super) struct UpstreamIo<T> {
    stream: T,
    read: Deadline,
    write: Deadline,
    read_timeout: Duration,
    write_timeout: Duration,
    read_idle: bool,
    multiplexed: bool,
}

impl<T> UpstreamIo<T> {
    pub(super) fn new(
        stream: T,
        read_timeout: Duration,
        write_timeout: Duration,
        multiplexed: bool,
    ) -> Self {
        Self {
            stream,
            read: Deadline::default(),
            write: Deadline::default(),
            read_timeout,
            write_timeout,
            read_idle: false,
            multiplexed,
        }
    }

    fn written(
        &mut self,
        result: Poll<io::Result<usize>>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<usize>> {
        match result {
            Poll::Pending => match self.write.check(cx, self.write_timeout) {
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                _ => Poll::Pending,
            },
            ready => {
                self.write.clear();
                // Upload progress extends the response deadline; empty flushes do not.
                if matches!(ready, Poll::Ready(Ok(n)) if n > 0) {
                    self.read.restart();
                }
                ready
            }
        }
    }
}

impl Connection for super::tls::Stream {
    fn connected(&self) -> Connected {
        if self.h2() {
            Connected::new().negotiated_h2()
        } else {
            Connected::new()
        }
    }
}

impl<T: Connection> Connection for UpstreamIo<T> {
    fn connected(&self) -> Connected {
        self.stream.connected()
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for UpstreamIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_read(cx, buf) {
            Poll::Pending if !self.read_idle && !self.multiplexed => {
                let timeout = self.read_timeout;
                self.read.check(cx, timeout)
            }
            ready => {
                self.read.clear();
                ready
            }
        }
    }
}

impl<T: AsyncRead + Unpin> hyper::rt::Read for UpstreamIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        hyper::rt::Read::poll_read(Pin::new(&mut TokioIo::new(self.get_mut())), cx, buf)
    }

    fn set_read_idle(self: Pin<&mut Self>, cx: &mut Context<'_>, idle: bool) {
        let this = self.get_mut();
        if this.read_idle != idle {
            // An idle socket has no response deadline. A new exchange must arm
            // a fresh deadline rather than inherit the preceding idle probe.
            this.read.clear();
            this.read_idle = idle;
        }
        if !idle && !this.multiplexed {
            let _ = this.read.check(cx, this.read_timeout);
        }
    }
}

impl<T: AsyncWrite + Unpin> hyper::rt::Write for UpstreamIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(self, cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(self, cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(self, cx)
    }
    fn is_write_vectored(&self) -> bool {
        AsyncWrite::is_write_vectored(self)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write_vectored(self, cx, bufs)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for UpstreamIo<T> {
    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        self.written(result, cx)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write_vectored(cx, bufs);
        self.written(result, cx)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_flush(cx) {
            Poll::Pending => {
                let timeout = self.write_timeout;
                self.write.check(cx, timeout)
            }
            ready => {
                self.write.clear();
                ready
            }
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_shutdown(cx) {
            Poll::Pending => {
                let timeout = self.write_timeout;
                self.write.check(cx, timeout)
            }
            ready => {
                self.write.clear();
                ready
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(start_paused = true)]
    async fn flush_without_upload_progress_cannot_extend_read_timeout() {
        let (client, _origin) = tokio::io::duplex(32);
        let timeout = Duration::from_millis(100);
        let mut client = UpstreamIo::new(client, timeout, timeout, false);
        let mut buf = [0; 1];
        assert!(client.read(&mut buf).now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(60)).await;
        client.flush().await.unwrap();
        // A zero-length write is not upload progress either.
        assert_eq!(client.write(&[]).await.unwrap(), 0);
        tokio::time::advance(Duration::from_millis(60)).await;
        assert_eq!(
            client.read(&mut buf).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn upload_progress_extends_read_deadline_and_success_rearms_it() {
        let (client, mut origin) = tokio::io::duplex(32);
        let timeout = Duration::from_millis(100);
        let mut client = UpstreamIo::new(client, timeout, timeout, false);
        let mut buf = [0; 1];
        assert!(client.read(&mut buf).now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(60)).await;
        assert_eq!(
            client
                .write_vectored(&[io::IoSlice::new(b"x")])
                .await
                .unwrap(),
            1
        );
        tokio::time::advance(Duration::from_millis(60)).await;
        assert!(client.read(&mut buf).now_or_never().is_none());
        origin.write_all(b"y").await.unwrap();
        assert_eq!(client.read(&mut buf).await.unwrap(), 1);
        assert_eq!(buf, *b"y");
        assert!(client.read(&mut buf).now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(110)).await;
        assert_eq!(
            client.read(&mut buf).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
