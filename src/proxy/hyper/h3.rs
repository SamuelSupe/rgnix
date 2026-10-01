use super::{
    Listener as RequestListener,
    body::{BoxError, Payload},
    io::Connection,
};
use crate::{
    model,
    runtime::{Limits, Shared},
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::{Buf, Bytes};
use http_body_util::BodyExt;
use hyper::body::Frame;
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) struct Listener {
    listener: Arc<RequestListener>,
    socket: Mutex<Option<std::net::UdpSocket>>,
    headers: Arc<tokio::sync::Semaphore>,
}
impl Listener {
    pub(crate) fn bind(
        shared: Arc<Shared>,
        config: &model::Listener,
        limits: &Limits,
    ) -> Result<Self> {
        let socket = std::net::UdpSocket::bind(config.address)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            listener: Arc::new(RequestListener {
                worker: 0,
                shared,
                address: config.address,
                socket: Arc::new(Mutex::new(None)),
                drain_timeout: Duration::from_secs(limits.shutdown_timeout_seconds),
                config: config.clone(),
            }),
            socket: Mutex::new(Some(socket)),
            headers: Arc::new(tokio::sync::Semaphore::new(limits.max_inflight)),
        })
    }
}

#[async_trait]
impl BackgroundService for Listener {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let result = self.run(&mut shutdown).await;
        if let Err(error) = result {
            log::error!("HTTP/3 listener: {error:#}");
        }
    }
}
impl Listener {
    async fn run(&self, shutdown: &mut ShutdownWatch) -> Result<()> {
        let snapshot = self.listener.shared.snapshot.load_full();
        let config = snapshot
            .hyper
            .as_ref()
            .unwrap()
            .quic
            .get(&self.listener.address)
            .unwrap()
            .clone();
        drop(snapshot);
        let socket = self
            .socket
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap();
        let endpoint = quinn::Endpoint::new(
            Default::default(),
            Some(config),
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                Some(result) = tasks.join_next(), if !tasks.is_empty() => { if let Err(error) = result { log::error!("HTTP/3 connection task: {error}"); } },
                incoming = endpoint.accept() => {
                    let Some(incoming) = incoming else { break; };
                    let Ok(lease) = self.listener.shared.hyper_admission.acquire(self.listener.address, incoming.remote_address().ip()) else { incoming.refuse(); continue; };
                    let snapshot = self.listener.shared.snapshot.load_full();
                    let config = snapshot.hyper.as_ref().unwrap().quic.get(&self.listener.address).unwrap().clone();
                    let header_timeout = snapshot.header_timeout(self.listener.address);
                    drop(snapshot);
                    let connecting = match incoming.accept_with(Arc::new(config)) {
                        Ok(connecting) => connecting,
                        Err(error) => {
                            self.listener.shared.telemetry.traffic.hyper_handshake_errors.inc();
                            log::debug!("HTTP/3 handshake: {error}");
                            continue;
                        }
                    };
                    let listener = self.listener.clone();
                    let headers = self.headers.clone();
                    let stop = shutdown.clone();
                    tasks.spawn(async move {
                        let _lease = lease.clone();
                        match tokio::time::timeout(Duration::from_secs(10), connecting).await {
                            Ok(Ok(connection)) => {
                                lease.established();
                                if let Err(error) = serve(connection, listener, headers, header_timeout, stop).await { log::debug!("HTTP/3 connection: {error:#}"); }
                            }
                            _ => { listener.shared.telemetry.traffic.hyper_handshake_errors.inc(); }
                        }
                    });
                }
            }
        }
        let drained = tokio::time::timeout(self.listener.drain_timeout, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tasks.abort_all();
        }
        endpoint.close(0u32.into(), b"shutdown");
        Ok(())
    }
}

async fn serve(
    connection: quinn::Connection,
    listener: Arc<RequestListener>,
    headers: Arc<tokio::sync::Semaphore>,
    header_timeout: Duration,
    mut shutdown: ShutdownWatch,
) -> Result<()> {
    let peer = connection.remote_address();
    let hello = connection
        .handshake_data()
        .and_then(|data| data.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .ok_or_else(|| anyhow::anyhow!("HTTP/3 TLS handshake metadata missing"))?;
    let snapshot = listener.shared.snapshot.load_full();
    // A resumed TLS handshake can skip certificate resolution. Apply expiry and
    // withdrawal checks before accepting application streams in that case too.
    let certificate = snapshot
        .tls_host(listener.address, hello.server_name.as_deref().unwrap_or(""))
        .and_then(|host| host.certificate.as_ref());
    if certificate.is_none_or(|cert| cert.valid_time().is_err()) {
        connection.close(0u32.into(), b"TLS certificate unavailable");
        anyhow::bail!("HTTP/3 certificate expired or withdrawn");
    }
    drop(snapshot);
    let mut state = Connection::default();
    state.tls = true;
    if let Some(identity) = connection.peer_identity().and_then(|p| {
        p.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
            .ok()
    }) && let Some(leaf) = identity.first()
    {
        let mut chain = openssl::stack::Stack::new()?;
        for cert in identity.iter().skip(1) {
            chain.push(openssl::x509::X509::from_der(cert.as_ref())?)?;
        }
        state.client_certificate = Some(Arc::new(crate::security::mtls::Peer {
            leaf: openssl::x509::X509::from_der(leaf.as_ref())?,
            chain,
        }));
    }
    let state = Arc::new(state);
    let mut server = h3::server::builder()
        .max_field_section_size(65536)
        .enable_extended_connect(true)
        .build(h3_quinn::Connection::new(connection.clone()))
        .await?;
    let mut streams = tokio::task::JoinSet::new();
    let mut draining = false;
    loop {
        tokio::select! {
            _ = shutdown.changed(), if !draining => { server.shutdown(0).await?; draining = true; }
            Some(result) = streams.join_next(), if !streams.is_empty() => { if let Err(error) = result { log::error!("HTTP/3 request task: {error}"); } },
            request = server.accept() => {
                let Some(resolver) = request? else { break; };
                let Ok(permit) = headers.clone().try_acquire_owned() else { connection.close(quinn::VarInt::from_u64(h3::error::Code::H3_EXCESSIVE_LOAD.value())?, b"header budget"); break; };
                let listener = listener.clone(); let state = state.clone();
                streams.spawn(async move {
                    let resolved = tokio::time::timeout(header_timeout, resolver.resolve_request()).await;
                    drop(permit);
                    match resolved {
                    Ok(Ok((mut request, stream))) => {
                        *request.version_mut() = http::Version::HTTP_3;
                        if let Err(error) = handle(request, stream, listener, peer, state).await { log::debug!("HTTP/3 stream: {error:#}"); }
                    },
                    Ok(Err(error)) => log::debug!("HTTP/3 request headers: {error}"),
                    Err(error) => log::debug!("HTTP/3 request header timeout: {error}"),
                    }
                });
            }
        }
    }
    while streams.join_next().await.is_some() {}
    Ok(())
}

async fn handle(
    mut request: http::Request<()>,
    stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    listener: Arc<RequestListener>,
    peer: std::net::SocketAddr,
    state: Arc<Connection>,
) -> Result<()> {
    if let Some(protocol) = request.extensions_mut().remove::<h3::ext::Protocol>() {
        request
            .extensions_mut()
            .insert(hyper::ext::Protocol::from(protocol.as_str()));
    }
    if request.method() == http::Method::CONNECT {
        return handle_tunnel(request, stream, listener, peer, state).await;
    }
    let expected = request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .map(|v| v.to_str()?.parse::<u64>().map_err(anyhow::Error::from))
        .transpose()?;
    let (mut send, receive) = stream.split();
    let input = futures::stream::try_unfold(
        (receive, false, 0u64),
        move |(mut receive, done, mut received)| async move {
            if done {
                return Ok::<_, BoxError>(None);
            }
            if let Some(mut data) = receive.recv_data().await? {
                let data = data.copy_to_bytes(data.remaining());
                received = received.saturating_add(data.len() as u64);
                if expected.is_some_and(|n| received > n) {
                    return Err("HTTP/3 request exceeds Content-Length".into());
                }
                return Ok(Some((Frame::data(data), (receive, false, received))));
            }
            if expected.is_some_and(|n| received != n) {
                return Err("incomplete HTTP/3 request body".into());
            }
            Ok(receive
                .recv_trailers()
                .await?
                .map(|trailers| (Frame::trailers(trailers), (receive, true, received))))
        },
    );
    let request =
        request.map(|_| Payload::Stream(Box::pin(http_body_util::StreamBody::new(input))));
    let mut request = request;
    let (sender, mut hints) = hyper::ext::SendInformational::channel();
    request.extensions_mut().insert(sender);
    let response = super::request::serve(&listener, request, peer, state);
    tokio::pin!(response);
    let mut hints_open = true;
    let response = loop {
        tokio::select! {
            biased;
            hint = std::future::poll_fn(|cx| hints.poll_recv(cx)), if hints_open => {
                if let Some(hint) = hint {
                    let timeout = hint.extensions().get::<hyper::ext::H2Timeouts>().map_or(Duration::from_secs(60), |t| t.write);
                    tokio::time::timeout(timeout, send.send_response(hint)).await??;
                } else { hints_open = false; }
            }
            response = &mut response => break response.unwrap(),
        }
    };
    drop(hints);
    let (parts, mut body) = response.into_parts();
    let settings = body.settings();
    let timeout = settings.map_or(Duration::from_secs(60), |s| s.send_timeout);
    tokio::time::timeout(
        timeout,
        send.send_response(http::Response::from_parts(parts, ())),
    )
    .await??;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(anyhow::Error::from_boxed)?;
        match frame.into_data() {
            Ok(data) => {
                tokio::time::timeout(timeout, send.send_data(data)).await??;
            }
            Err(frame) => {
                if let Ok(trailers) = frame.into_trailers() {
                    tokio::time::timeout(timeout, send.send_trailers(trailers)).await??;
                }
            }
        }
    }
    tokio::time::timeout(timeout, send.finish()).await??;
    body.delivered();
    Ok(())
}

async fn handle_tunnel(
    mut request: http::Request<()>,
    stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    listener: Arc<RequestListener>,
    peer: std::net::SocketAddr,
    state: Arc<Connection>,
) -> Result<()> {
    let (ready, upgrade) = tokio::sync::oneshot::channel();
    let (delivered, delivery) = tokio::sync::oneshot::channel();
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    request
        .extensions_mut()
        .insert(super::tunnel::H3Upgrade(Arc::new(Mutex::new(Some(
            super::tunnel::H3Transport {
                ready: upgrade,
                delivered: delivery,
                closed: closed.clone(),
            },
        )))));
    let response = super::request::serve(
        &listener,
        request.map(|_| Payload::Full(http_body_util::Full::new(Bytes::new()))),
        peer,
        state,
    )
    .await
    .unwrap();
    let accepted = response
        .extensions()
        .get::<super::tunnel::H3Accepted>()
        .cloned();
    let (parts, mut body) = response.into_parts();
    let (mut send, mut receive) = stream.split();
    let timeout = accepted
        .as_ref()
        .map_or(Duration::from_secs(60), |s| s.write);
    tokio::time::timeout(
        timeout,
        send.send_response(http::Response::from_parts(parts, ())),
    )
    .await??;
    let Some(settings) = accepted else {
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.map_err(anyhow::Error::from_boxed)?.into_data() {
                tokio::time::timeout(timeout, send.send_data(data)).await??;
            }
        }
        tokio::time::timeout(timeout, send.finish()).await??;
        body.delivered();
        receive.stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
        return Ok(());
    };
    let (client, bridge) = tokio::io::duplex(65536);
    ready
        .send(client)
        .map_err(|_| anyhow::anyhow!("HTTP/3 tunnel cancelled before acceptance"))?;
    let (mut reader, mut writer) = tokio::io::split(bridge);
    let incoming = async {
        use tokio::io::AsyncWriteExt;
        while let Some(mut data) =
            tokio::time::timeout(settings.read, receive.recv_data()).await??
        {
            while data.has_remaining() {
                let bytes = data.chunk();
                tokio::time::timeout(settings.read, writer.write_all(bytes)).await??;
                let n = bytes.len();
                data.advance(n);
            }
        }
        writer.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    };
    let outgoing = async {
        use tokio::io::AsyncReadExt;
        let mut buffer = vec![0; 16384];
        loop {
            let n = reader.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            tokio::time::timeout(
                settings.write,
                send.send_data(Bytes::copy_from_slice(&buffer[..n])),
            )
            .await??;
        }
        // Dropping a failed duplex bridge also produces EOF. Only a completed
        // upstream half-close is an orderly QUIC FIN; errors require RESET_STREAM.
        if !closed.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("HTTP/3 tunnel transport failed");
        }
        tokio::time::timeout(settings.write, send.finish()).await??;
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::try_join!(incoming, outgoing);
    if result.is_err() {
        send.stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
        receive.stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
    }
    let _ = delivered.send(result.is_ok());
    result?;
    Ok(())
}
