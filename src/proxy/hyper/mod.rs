mod body;
mod io;
mod request;

use crate::{
    model::*,
    runtime::{Limits, Shared},
};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use hyper_timeout::TimeoutConnector;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioTimer},
};
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

type ProxyClient = Client<TimeoutConnector<HttpConnector>, body::RequestBody>;
type Timeouts = (Duration, Duration, Duration, Duration);

#[derive(Default)]
struct Clients {
    version: u64,
    clients: HashMap<Timeouts, ProxyClient>,
}

#[derive(Clone)]
pub(crate) struct Listener {
    shared: Arc<Shared>,
    address: SocketAddr,
    socket: Arc<Mutex<Option<std::net::TcpListener>>>,
    clients: Arc<Mutex<Clients>>,
    pool_size: usize,
    drain_timeout: Duration,
}

pub(crate) fn validate(snapshot: &RuntimeSnapshot) -> Result<()> {
    ensure!(
        snapshot.gateway.is_none(),
        "experimental Hyper does not support Gateway routing"
    );
    for listener in &snapshot.listeners {
        ensure!(
            !listener.tls && !listener.http2 && !listener.proxy_protocol,
            "experimental Hyper requires plain HTTP/1 listeners without PROXY protocol: {}",
            listener.address
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
                route.script.is_none() && s.body_policy.inspection == crate::body::Inspection::Off,
                "experimental Hyper does not support RGL/body inspection: {}",
                route.id
            );
            ensure!(
                route.tenant.is_none() && route.rollout.is_none() && s.gateway.is_none(),
                "experimental Hyper does not support tenant/rollout/Gateway policy: {}",
                route.id
            );
            ensure!(
                s.security.jwt.is_none()
                    && s.security.external.is_none()
                    && s.security.mtls.mode == crate::security::mtls::Mode::Off,
                "experimental Hyper does not support authentication: {}",
                route.id
            );
            ensure!(
                s.compression.gzip == 0
                    && s.compression.brotli == 0
                    && s.alias.is_none()
                    && s.try_files.is_empty()
                    && !matches!(route.action, Action::Static),
                "experimental Hyper does not support static files/compression: {}",
                route.id
            );
            ensure!(
                s.upstream.protocol == crate::upstream::Protocol::Http1,
                "experimental Hyper requires HTTP/1 upstreams: {}",
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
    for (name, backend) in &snapshot.backends {
        ensure!(
            !backend.tls && backend.profile.protocol == crate::upstream::Protocol::Http1,
            "experimental Hyper requires plain HTTP/1 backend: {name}"
        );
        ensure!(
            !matches!(backend.options.balance, crate::backend::Balance::Sticky(_)),
            "experimental Hyper does not support sticky cookies: {name}"
        );
    }
    Ok(())
}

impl Listener {
    pub(crate) fn bind(shared: Arc<Shared>, address: SocketAddr, limits: &Limits) -> Result<Self> {
        let socket = std::net::TcpListener::bind(address)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            shared,
            address,
            socket: Arc::new(Mutex::new(Some(socket))),
            clients: Arc::new(Mutex::new(Clients::default())),
            pool_size: limits.upstream_keepalive_pool_size * limits.threads,
            drain_timeout: Duration::from_secs(limits.shutdown_timeout_seconds),
        })
    }

    fn client(&self, version: u64, settings: &Settings) -> ProxyClient {
        let key = (
            settings.connect_timeout,
            settings.read_timeout,
            settings.write_timeout,
            settings.keepalive,
        );
        let build = || {
            let mut connector = HttpConnector::new();
            connector.set_nodelay(true);
            connector.set_connect_timeout(Some(key.0));
            let mut connector = TimeoutConnector::new(connector);
            connector.set_read_timeout(Some(key.1));
            connector.set_write_timeout(Some(key.2));
            connector.set_reset_reader_on_write(true);
            Client::builder(TokioExecutor::new())
                .pool_timer(TokioTimer::new())
                .pool_idle_timeout(key.3)
                .pool_max_idle_per_host(self.pool_size)
                .retry_canceled_requests(false)
                .build(connector)
        };
        let mut cache = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if version < cache.version {
            return build();
        }
        if version > cache.version {
            cache.clients.clear();
            cache.version = version;
        }
        cache.clients.entry(key).or_insert_with(build).clone()
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
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => {},
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let _ = stream.set_nodelay(true);
                            let stop = shutdown.clone();
                            let listener = self.clone();
                            connections.spawn(async move {
                                let state = Arc::new(io::Connection::default());
                                let service = hyper::service::service_fn(|request| request::serve(&listener, request, peer, state.clone()));
                                let stream = io::TrackedIo::new(stream, state.clone());
                                let mut builder = hyper::server::conn::http1::Builder::new();
                                builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(60));
                                let connection = builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service);
                                tokio::pin!(connection);
                                let mut stop = stop;
                                tokio::select! {
                                    _ = &mut connection => {},
                                    _ = stop.changed() => {
                                        connection.as_mut().graceful_shutdown();
                                        let _ = connection.await;
                                    }
                                }
                            });
                        }
                        Err(error) => { log::error!("Hyper accept: {error}"); break; }
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
