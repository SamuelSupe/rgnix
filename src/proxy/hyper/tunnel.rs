use super::{
    body::{Counters, RequestGuard},
    upstream::UpstreamIo,
};
use std::{
    io,
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(super) struct Tunnel {
    pub downstream: Downstream,
    pub upstream: Peer,
}

pub(super) enum Downstream {
    Hyper(hyper::upgrade::OnUpgrade),
    #[cfg(feature = "http3")]
    H3(H3Transport),
}

#[cfg(feature = "http3")]
pub(super) struct H3Transport {
    pub ready: tokio::sync::oneshot::Receiver<tokio::io::DuplexStream>,
    pub delivered: tokio::sync::oneshot::Receiver<bool>,
    pub closed: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(feature = "http3")]
#[derive(Clone)]
pub(super) struct H3Upgrade(pub Arc<std::sync::Mutex<Option<H3Transport>>>);

#[cfg(feature = "http3")]
#[derive(Clone)]
pub(super) struct H3Accepted {
    pub read: std::time::Duration,
    pub write: std::time::Duration,
}

pub(super) fn downstream<B>(request: &mut http::Request<B>) -> pingora::Result<Downstream> {
    #[cfg(feature = "http3")]
    if request.version() == http::Version::HTTP_3 {
        return request
            .extensions_mut()
            .remove::<H3Upgrade>()
            .and_then(|upgrade| upgrade.0.lock().unwrap_or_else(|e| e.into_inner()).take())
            .map(Downstream::H3)
            .ok_or_else(|| super::super::error(500, "HTTP/3 tunnel transport missing"));
    }
    Ok(Downstream::Hyper(hyper::upgrade::on(request)))
}

pub(super) enum Peer {
    Upgrade(hyper::upgrade::OnUpgrade),
    Tcp(tokio::net::TcpStream),
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(super) async fn run(tunnel: Tunnel, mut guard: Box<RequestGuard>) {
    let settings = &guard.ctx.route.as_ref().unwrap().settings;
    let (client_read, client_write, upstream_read, upstream_write) = (
        settings.client_body_timeout,
        settings.send_timeout,
        settings.read_timeout,
        settings.write_timeout,
    );
    let result = async {
        #[cfg(feature = "http3")]
        let mut delivered = None;
        #[cfg(feature = "http3")]
        let mut closed = None;
        let downstream: Box<dyn Io> = match tunnel.downstream {
            Downstream::Hyper(upgrade) => Box::new(hyper_util::rt::TokioIo::new(upgrade.await?)),
            #[cfg(feature = "http3")]
            Downstream::H3(upgrade) => {
                delivered = Some(upgrade.delivered);
                closed = Some(upgrade.closed);
                Box::new(upgrade.ready.await?)
            }
        };
        let mut downstream = UpstreamIo::new(
            Counted {
                stream: downstream,
                counts: guard.counts.clone(),
                #[cfg(feature = "http3")]
                closed,
            },
            client_read,
            client_write,
            false,
        );
        let result = match tunnel.upstream {
            Peer::Upgrade(upstream) => {
                let mut upstream = UpstreamIo::new(
                    hyper_util::rt::TokioIo::new(upstream.await?),
                    upstream_read,
                    upstream_write,
                    false,
                );
                tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await
            }
            Peer::Tcp(upstream) => {
                let mut upstream = UpstreamIo::new(upstream, upstream_read, upstream_write, false);
                tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await
            }
        };
        drop(downstream);
        result?;
        #[cfg(feature = "http3")]
        if let Some(delivered) = delivered
            && !tokio::time::timeout(client_write, delivered).await??
        {
            return Err("HTTP/3 tunnel response was not delivered".into());
        }
        Ok::<_, super::body::BoxError>(())
    }
    .await;
    if let Err(error) = &result {
        guard.error = Some(
            pingora::Error::explain(
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
                {
                    pingora::ErrorType::ReadTimedout
                } else {
                    pingora::ErrorType::ConnectionClosed
                },
                error.to_string(),
            )
            .into_down(),
        );
    }
    guard.finish(result.is_ok());
}

// Count progress as it happens so cancellation and timeouts retain partial bytes.
struct Counted<T> {
    stream: T,
    counts: Arc<Counters>,
    #[cfg(feature = "http3")]
    closed: Option<Arc<std::sync::atomic::AtomicBool>>,
}
impl<T: AsyncRead + Unpin> AsyncRead for Counted<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        self.counts
            .request
            .fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        result
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for Counted<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = result {
            self.counts.response.fetch_add(n as u64, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.stream).poll_shutdown(cx);
        #[cfg(feature = "http3")]
        if let Poll::Ready(Ok(())) = result
            && let Some(closed) = &self.closed
        {
            closed.store(true, Ordering::Release);
        }
        result
    }
}
