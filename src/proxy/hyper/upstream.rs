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
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};
use tower_service::Service;

#[derive(Clone)]
pub(super) struct Connector {
    http: HttpConnector,
    read: Duration,
    write: Duration,
}

impl Connector {
    pub fn new(connect: Duration, read: Duration, write: Duration) -> Self {
        let mut http = HttpConnector::new();
        http.set_nodelay(true);
        http.set_connect_timeout(Some(connect));
        Self { http, read, write }
    }
}

impl Service<Uri> for Connector {
    type Response = TokioIo<UpstreamIo<TcpStream>>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connecting = self.http.call(uri);
        let (read, write) = (self.read, self.write);
        Box::pin(async move {
            let stream = connecting.await?.into_inner();
            Ok(TokioIo::new(UpstreamIo::new(stream, read, write)))
        })
    }
}

pub(super) struct UpstreamIo<T> {
    stream: T,
    read: Deadline,
    write: Deadline,
    read_timeout: Duration,
    write_timeout: Duration,
}

impl<T> UpstreamIo<T> {
    fn new(stream: T, read_timeout: Duration, write_timeout: Duration) -> Self {
        Self {
            stream,
            read: Deadline::default(),
            write: Deadline::default(),
            read_timeout,
            write_timeout,
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
            Poll::Pending => {
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
        let mut client = UpstreamIo::new(client, timeout, timeout);
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
        let mut client = UpstreamIo::new(client, timeout, timeout);
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
