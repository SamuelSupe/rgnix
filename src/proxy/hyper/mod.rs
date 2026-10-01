pub(crate) mod admission;
mod body;
mod h2_headers;
mod h2c;
#[cfg(feature = "http3")]
pub(crate) mod h3;
#[cfg(feature = "http3")]
mod h3_tls;
mod io;
mod plain;
mod prepared;
mod request;
mod responses;
mod tls;
mod tunnel;
mod upstream;
mod websocket;
pub(crate) use prepared::Prepared;

use crate::{
    model::*,
    runtime::{Limits, Shared},
};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use hyper_util::{client::legacy::Client, rt::TokioTimer};
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

type ProxyClient = Client<upstream::Connector, body::RequestBody>;
#[derive(Clone)]
pub(crate) struct Listener {
    pub(crate) worker: usize,
    shared: Arc<Shared>,
    address: SocketAddr,
    socket: Arc<Mutex<Option<std::net::TcpListener>>>,
    drain_timeout: Duration,
    config: crate::model::Listener,
}

pub(crate) fn validate(snapshot: &RuntimeSnapshot) -> Result<()> {
    for listener in &snapshot.listeners {
        let enabled = snapshot
            .hosts
            .iter()
            .filter(|h| h.listener == listener.address)
            .flat_map(|h| &h.routes)
            .any(|r| r.settings.http3);
        ensure!(
            !enabled || cfg!(feature = "http3"),
            "HTTP/3 requires the http3 build feature"
        );
        ensure!(
            !enabled || (listener.tls && !listener.proxy_protocol && listener.address.port() != 0),
            "HTTP/3 requires TLS, a fixed UDP port, and no PROXY protocol"
        );
    }
    for host in &snapshot.hosts {
        for route in &host.routes {
            let s = &route.settings;
            ensure!(
                !matches!(route.action, Action::Return { status, .. } if status < 200),
                "experimental Hyper cannot use informational status as a final response: {}",
                route.id
            );
            ensure!(
                s.request_headers.iter().all(|(k, _)| ![
                    "connection",
                    "upgrade",
                    "content-length",
                    "transfer-encoding",
                    "trailer"
                ]
                .contains(&k.to_ascii_lowercase().as_str())),
                "experimental Hyper cannot override request framing headers: {}",
                route.id
            );
            ensure!(
                s.response_headers.iter().all(|h| ![
                    "connection",
                    "upgrade",
                    "content-length",
                    "transfer-encoding",
                    "trailer"
                ]
                .contains(&h.name.to_ascii_lowercase().as_str())),
                "experimental Hyper cannot override response framing headers: {}",
                route.id
            );
        }
    }
    Ok(())
}

impl Listener {
    pub(crate) fn bind(
        shared: Arc<Shared>,
        config: &crate::model::Listener,
        limits: &Limits,
    ) -> Result<Vec<Self>> {
        let address = config.address;
        let mut listeners = Vec::with_capacity(limits.threads);
        #[cfg(target_os = "linux")]
        let mut bind_address = address;
        for worker in 0..limits.threads {
            #[cfg(target_os = "linux")]
            let socket = {
                let socket = socket2::Socket::new(
                    socket2::Domain::for_address(address),
                    socket2::Type::STREAM,
                    Some(socket2::Protocol::TCP),
                )?;
                socket.set_reuse_address(true)?;
                socket.set_reuse_port(worker > 0)?;
                socket.set_nonblocking(true)?;
                socket.bind(&bind_address.into())?;
                // The first bind stays exclusive; only this listener's workers join its port.
                socket.set_reuse_port(limits.threads > 1)?;
                socket.listen(1024)?;
                let socket = std::net::TcpListener::from(socket);
                bind_address = socket.local_addr()?;
                socket
            };
            #[cfg(not(target_os = "linux"))]
            let socket = if let Some(listener) = listeners.first() {
                let listener: &Self = listener;
                listener
                    .socket
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .try_clone()?
            } else {
                let socket = std::net::TcpListener::bind(address)?;
                socket.set_nonblocking(true)?;
                socket
            };
            listeners.push(Self {
                worker,
                shared: shared.clone(),
                address,
                socket: Arc::new(Mutex::new(Some(socket))),
                drain_timeout: Duration::from_secs(limits.shutdown_timeout_seconds),
                config: config.clone(),
            });
        }
        Ok(listeners)
    }
}

#[async_trait]
impl BackgroundService for Listener {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let socket = self
            .socket
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap();
        let listener = match tokio::net::TcpListener::from_std(socket) {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("Hyper listener {}: {error}", self.address);
                return;
            }
        };
        let request_listener = Arc::new(self.clone());
        let mut connections = tokio::task::JoinSet::new();
        let mut backoff = Duration::from_millis(25);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = result { log::error!("Hyper connection task: {error}"); }
                },
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            backoff = Duration::from_millis(25);
                            let Ok(lease) = self.shared.hyper_admission.acquire(self.address, peer.ip()) else { continue; };
                            let _ = stream.set_nodelay(true);
                            let stop = shutdown.clone();
                            let listener = request_listener.clone();
                            connections.spawn(async move {
                                let _lease = lease.clone();
                                let mut state = io::Connection::default();
                                state.tls = listener.config.tls;
                                state.admission = Some(lease);
                                let mut stream = stream;
                                if listener.config.proxy_protocol {
                                    let trusted = listener.config.proxy_trusted.iter().any(|n| n.contains(&peer.ip()));
                                    if !trusted { return; }
                                    match tokio::time::timeout(Duration::from_secs(2), crate::proxy_protocol::read(&mut stream)).await {
                                        Ok(Ok(address)) => state.proxy_address = address,
                                        _ => return,
                                    }
                                }
                                let snapshot = listener.shared.snapshot.load_full();
                                let acceptor = snapshot.hyper.as_ref().unwrap().acceptors.get(&listener.address).cloned();
                                let header_timeout = snapshot.header_timeout(listener.address);
                                state.read_timeout_ms.store(header_timeout.as_millis().min(u64::MAX as u128) as u64, std::sync::atomic::Ordering::Relaxed);
                                drop(snapshot);
                                let stream = if let Some(acceptor) = &acceptor {
                                    match tls::accept(acceptor, stream).await {
                                        Ok(stream) => {
                                            state.client_certificate = crate::security::mtls::peer(stream.ssl());
                                            tls::Stream::Tls(Box::new(stream))
                                        }
                                        Err(error) => { listener.shared.telemetry.traffic.hyper_handshake_errors.inc(); log::debug!("Hyper TLS handshake: {error:#}"); return; }
                                    }
                                } else { tls::Stream::Plain(stream) };
                                let h2 = stream.h2() || (!listener.config.tls && listener.config.http2);
                                let state = Arc::new(state);
                                let request_listener = listener.clone();
                                let request_state = state.clone();
                                let upgrade_stop = stop.clone();
                                let service = hyper::service::service_fn(move |request| {
                                    let listener = request_listener.clone();
                                    let state = request_state.clone();
                                    let stop = upgrade_stop.clone();
                                    async move {
                                        if let Some(lease) = &state.admission { lease.established(); }
                                        if request.headers().get(http::header::UPGRADE).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"h2c")) {
                                            Ok(h2c::upgrade(listener, request, peer, state, stop).await)
                                        } else { request::serve(&listener, request.map(body::Payload::Incoming), peer, state).await }
                                    }
                                });
                                let stream = io::TrackedIo::new(stream, state.clone(), header_timeout);
                                let mut builder = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
                                builder.http1().timer(TokioTimer::new()).header_read_timeout(header_timeout).max_buf_size(65536);
                                builder.http2().timer(TokioTimer::new()).enable_connect_protocol();
                                let builder = if h2 && listener.config.tls { builder.http2_only() } else if h2 { builder } else { builder.http1_only() };
                                let connection = builder.serve_connection_with_upgrades(stream, service);
                                tokio::pin!(connection);
                                let mut stop = stop;
                                tokio::select! {
                                    result = &mut connection => {
                                        if let Err(error) = result { log::debug!("Hyper connection {peer}: {error}"); }
                                    },
                                    _ = stop.changed() => {
                                        connection.as_mut().graceful_shutdown();
                                        let _ = connection.await;
                                    }
                                }
                                state.wait_tunnel().await;
                            });
                        }
                        Err(error) => {
                            let reason = admission::accept_error(&error).unwrap_or("fatal");
                            self.shared.telemetry.traffic.hyper_accept_errors.with_label_values(&[reason]).inc();
                            if reason == "fatal" { log::error!("Hyper accept: {error}"); break; }
                            log::warn!("Hyper accept temporarily unavailable: {error}");
                            tokio::select! { _ = shutdown.changed() => break, _ = tokio::time::sleep(backoff) => {} }
                            backoff = (backoff * 2).min(Duration::from_secs(1));
                        }
                    }
                }
            }
        }
        drop(listener);
        let _ = tokio::time::timeout(self.drain_timeout, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
    }
}
