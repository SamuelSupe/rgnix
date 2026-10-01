use super::{ProxyClient, upstream::Connector};
use crate::{
    model::{Action, Route, RuntimeSnapshot},
    proxy::{Context, error, expand},
};
use anyhow::{Context as _, Result};
use http::{HeaderName, HeaderValue, uri::Authority};
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioTimer},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, OnceLock},
};

pub(crate) struct Prepared {
    pub acceptors: HashMap<SocketAddr, openssl::ssl::SslAcceptor>,
    #[cfg(feature = "http3")]
    pub quic: HashMap<SocketAddr, quinn::ServerConfig>,
    #[cfg(feature = "http3")]
    pub http3_all: bool,
    pub has_plain: bool,
    pub slash_redirects: bool,
    pub pool_size: usize,
    pub workers: usize,
    pub idle_budget: hyper_util::client::legacy::IdlePoolBudget,
    // Route IDs need not be unique (exact and prefix locations can share a path).
    // These identities remain valid while the owning snapshot retains its routes.
    routes: HashMap<(SocketAddr, usize), PreparedRoute>,
    clients: HashMap<ClientKey, Arc<Clients>>,
    tls_keys: HashMap<SocketAddr, [u8; 32]>,
}

pub(super) struct PreparedRoute {
    pub plain: Option<super::plain::Action>,
    pub targets: HashMap<String, PreparedTarget>,
    pub request_headers: Vec<(HeaderName, Value)>,
    pub response_headers: Vec<(HeaderName, Value, bool)>,
    // Dynamic templates retain the complete original view; fixed routes capture
    // only fields read by policy and completion, before outbound header edits.
    pub original_headers: Option<Vec<HeaderName>>,
}

#[derive(Clone)]
pub(super) struct PreparedTarget {
    pub backend: Arc<PreparedBackend>,
    clients: Arc<Clients>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    name: String,
    listener: SocketAddr,
    connect: std::time::Duration,
    read: std::time::Duration,
    write: std::time::Duration,
    keepalive: std::time::Duration,
    transport: [u8; 32],
    backend: usize,
}

struct Clients {
    builder: hyper_util::client::legacy::Builder,
    connector: Connector,
    workers: Vec<OnceLock<ProxyClient>>,
    websocket_connector: Option<Connector>,
    websocket_workers: Vec<OnceLock<ProxyClient>>,
}

impl PreparedTarget {
    pub fn websocket_client(&self, worker: usize) -> &ProxyClient {
        match &self.clients.websocket_connector {
            Some(connector) => self.clients.websocket_workers[worker]
                .get_or_init(|| self.clients.builder.build(connector.clone())),
            None => self.client(worker),
        }
    }
    pub fn client(&self, worker: usize) -> &ProxyClient {
        // HTTP/1 pools stay on their reactor; HTTP/2 and ALPN auto retain one
        // shared multiplexer. Transport validation stays on the control plane.
        let worker = if self.clients.workers.len() == 1 {
            0
        } else {
            worker
        };
        self.clients.workers[worker]
            .get_or_init(|| self.clients.builder.build(self.clients.connector.clone()))
    }
}

pub(super) struct PreparedBackend {
    pub backend: Arc<crate::backend::Backend>,
    pub host: HeaderValue,
    authorities: HashMap<SocketAddr, Authority>,
    metrics: OnceLock<Option<crate::telemetry::BackendMetrics>>,
}

impl PreparedBackend {
    pub fn metrics(
        &self,
        telemetry: &crate::telemetry::Telemetry,
        name: &str,
    ) -> Option<&crate::telemetry::BackendMetrics> {
        self.metrics
            .get_or_init(|| crate::telemetry::BackendMetrics::new(telemetry, name))
            .as_ref()
    }

    pub fn authority(&self, address: SocketAddr) -> Result<Authority, http::uri::InvalidUri> {
        match self.authorities.get(&address) {
            Some(authority) => Ok(authority.clone()),
            // DNS maintenance can introduce endpoints after this snapshot was prepared.
            None => address.to_string().parse(),
        }
    }
}

pub(super) enum Value {
    Constant(HeaderValue),
    Variable(String),
}

impl Value {
    fn new(value: &str) -> Result<Self> {
        Ok(if value.contains('$') {
            Self::Variable(value.into())
        } else {
            Self::Constant(value.parse()?)
        })
    }

    pub fn expand(&self, ctx: &Context, host: &str) -> pingora::Result<HeaderValue> {
        match self {
            Self::Constant(value) => Ok(value.clone()),
            Self::Variable(value) => expand(value, ctx, false, host)
                .parse()
                .map_err(|_| error(500, "invalid expanded header")),
        }
    }
}

impl Prepared {
    pub fn new(
        snapshot: &RuntimeSnapshot,
        previous: Option<&Self>,
        pool_size: usize,
        workers: usize,
        idle_budget: hyper_util::client::legacy::IdlePoolBudget,
        telemetry: &Arc<crate::telemetry::Telemetry>,
        #[cfg(feature = "http3")] http3_all: bool,
    ) -> Result<Self> {
        let mut clients: HashMap<ClientKey, Arc<Clients>> = HashMap::new();
        let backends: HashMap<_, _> = snapshot
            .backends
            .iter()
            .map(|(name, backend)| {
                let authorities = backend
                    .endpoints
                    .iter()
                    .map(|endpoint| Ok((endpoint.address, endpoint.address.to_string().parse()?)))
                    .collect::<Result<_>>()?;
                Ok((
                    name.as_str(),
                    Arc::new(PreparedBackend {
                        backend: backend.clone(),
                        host: backend.host_header.parse()?,
                        authorities,
                        metrics: OnceLock::new(),
                    }),
                ))
            })
            .collect::<Result<_>>()?;
        let mut routes = HashMap::new();
        for host in &snapshot.hosts {
            for route in &host.routes {
                let settings = &route.settings;
                if routes.contains_key(&(host.listener, Arc::as_ptr(route) as usize)) {
                    continue;
                }
                let mut names = std::collections::BTreeSet::new();
                if route.script.is_some() {
                    names.extend(route.allowed_backends.values().cloned());
                }
                if let Action::Proxy { backend, .. } = &route.action {
                    names.insert(backend.clone());
                }
                if let Some(gateway) = &settings.gateway {
                    names.extend(gateway.backends.iter().filter_map(|(name, _)| name.clone()));
                }
                if let Some(rollout) = &route.rollout {
                    names.extend(rollout.backends.values().cloned());
                }
                let mut targets = HashMap::new();
                for name in names {
                    let Some(backend) = backends.get(name.as_str()) else {
                        continue;
                    };
                    let transport = if settings.gateway.is_some() {
                        &backend.backend.profile
                    } else {
                        &settings.upstream
                    };
                    // A reused backend is the same validated configuration, including
                    // endpoint membership. Changed credentials or transport never share pools.
                    let key = ClientKey {
                        name: name.clone(),
                        listener: host.listener,
                        connect: settings.connect_timeout,
                        read: settings.read_timeout,
                        write: settings.write_timeout,
                        keepalive: settings.keepalive,
                        transport: Sha256::digest(serde_json::to_vec(transport)?).into(),
                        backend: Arc::as_ptr(&backend.backend) as usize,
                    };
                    let client = if let Some(client) = clients
                        .get(&key)
                        .or_else(|| previous.and_then(|p| p.clients.get(&key)))
                    {
                        client.clone()
                    } else {
                        let client_workers =
                            if transport.protocol == crate::upstream::Protocol::Http1 {
                                workers
                            } else {
                                1
                            };
                        let mut builder = Client::builder(TokioExecutor::new());
                        builder
                            .pool_timer(TokioTimer::new())
                            .pool_idle_timeout(settings.keepalive)
                            .pool_max_idle_per_host(pool_size / client_workers)
                            .shared_idle_budget(idle_budget.clone())
                            .retry_canceled_requests(false)
                            .http2_only(transport.protocol == crate::upstream::Protocol::Http2);
                        let telemetry = telemetry.clone();
                        let label = name.clone();
                        let observed = backend.clone();
                        builder.connection_observer(move |reused, elapsed| {
                            if let Some(metrics) = observed.metrics(&telemetry, &label) {
                                metrics.connect[usize::from(reused)].observe(elapsed.as_secs_f64());
                                return;
                            }
                            let label = telemetry.label("backend", &label);
                            telemetry
                                .traffic
                                .upstream_connect
                                .with_label_values(&[label, if reused { "true" } else { "false" }])
                                .observe(elapsed.as_secs_f64());
                        });
                        let connector = Connector::new(
                            settings.connect_timeout,
                            settings.read_timeout,
                            settings.write_timeout,
                            transport,
                            &backend.backend,
                        )?;
                        let websocket_connector =
                            if transport.protocol == crate::upstream::Protocol::Auto {
                                let mut transport = transport.clone();
                                transport.protocol = crate::upstream::Protocol::Http1;
                                Some(Connector::new(
                                    settings.connect_timeout,
                                    settings.read_timeout,
                                    settings.write_timeout,
                                    &transport,
                                    &backend.backend,
                                )?)
                            } else {
                                None
                            };
                        Arc::new(Clients {
                            builder,
                            connector,
                            workers: (0..client_workers).map(|_| OnceLock::new()).collect(),
                            websocket_workers: if websocket_connector.is_some() {
                                (0..workers).map(|_| OnceLock::new()).collect()
                            } else {
                                Vec::new()
                            },
                            websocket_connector,
                        })
                    };
                    clients.entry(key).or_insert_with(|| client.clone());
                    targets.insert(
                        name,
                        PreparedTarget {
                            backend: backend.clone(),
                            clients: client,
                        },
                    );
                }
                let request_headers = settings
                    .request_headers
                    .iter()
                    .map(|(name, value)| Ok((name.parse()?, Value::new(value)?)))
                    .collect::<Result<_>>()?;
                let response_headers = settings
                    .response_headers
                    .iter()
                    .map(|header| {
                        Ok((
                            header.name.parse()?,
                            Value::new(&header.value)?,
                            header.always,
                        ))
                    })
                    .collect::<Result<_>>()?;
                let backend = match &route.action {
                    Action::Proxy { backend, .. } => Some(
                        backends
                            .get(backend.as_str())
                            .context("missing prepared Hyper backend")?
                            .clone(),
                    ),
                    _ => None,
                };
                let original_headers = original_headers(route, backend.as_deref());
                let plain = super::plain::Action::prepare(
                    route,
                    match &route.action {
                        Action::Proxy { backend, .. } => targets.get(backend),
                        _ => None,
                    },
                )?;
                routes.insert(
                    (host.listener, Arc::as_ptr(route) as usize),
                    PreparedRoute {
                        plain,
                        targets,
                        request_headers,
                        response_headers,
                        original_headers,
                    },
                );
            }
        }
        let mut acceptors = HashMap::new();
        let mut tls_keys = HashMap::new();
        #[cfg(feature = "http3")]
        let mut quic = HashMap::new();
        for listener in snapshot.listeners.iter().filter(|l| l.tls) {
            let key = tls_key(snapshot, listener)?;
            let prior = previous.filter(|p| p.tls_keys.get(&listener.address) == Some(&key));
            let acceptor = match prior.and_then(|p| p.acceptors.get(&listener.address)) {
                Some(acceptor) => acceptor.clone(),
                None => super::tls::acceptor(snapshot, listener)?,
            };
            acceptors.insert(listener.address, acceptor);
            tls_keys.insert(listener.address, key);
            #[cfg(feature = "http3")]
            if http3_all
                || snapshot
                    .hosts
                    .iter()
                    .filter(|h| h.listener == listener.address)
                    .flat_map(|h| &h.routes)
                    .any(|r| r.settings.http3)
            {
                let config = match prior.and_then(|p| p.quic.get(&listener.address)) {
                    Some(config) => config.clone(),
                    None => super::h3_tls::config(snapshot, listener.address)?,
                };
                quic.insert(listener.address, config);
            }
        }
        Ok(Self {
            acceptors,
            #[cfg(feature = "http3")]
            quic,
            #[cfg(feature = "http3")]
            http3_all,
            has_plain: routes.values().any(|r| r.plain.is_some()),
            slash_redirects: snapshot.hosts.iter().flat_map(|h| &h.routes).any(|r| {
                matches!(&r.matcher, crate::model::PathMatch::NginxPrefix(p) if p.len() > 1 && p.ends_with('/'))
                    && matches!(r.action, Action::Proxy { .. })
            }),
            pool_size,
            workers,
            routes,
            idle_budget,
            clients,
            tls_keys,
        })
    }

    pub(super) fn route(&self, listener: SocketAddr, route: &Arc<Route>) -> &PreparedRoute {
        &self.routes[&(listener, Arc::as_ptr(route) as usize)]
    }
}

fn tls_key(snapshot: &RuntimeSnapshot, listener: &crate::model::Listener) -> Result<[u8; 32]> {
    let mut hosts: Vec<_> = snapshot
        .certificates
        .iter()
        .filter(|h| h.listener == listener.address)
        .collect();
    hosts.sort_by(|a, b| a.name.cmp(&b.name));
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(listener)?);
    for host in hosts {
        hash.update(serde_json::to_vec(&(
            &host.name,
            host.ingress,
            host.default,
            &host.client_auth,
        ))?);
        if let Some(cert) = &host.certificate {
            let chain: Vec<_> = std::iter::once(&cert.leaf)
                .chain(cert.chain.iter())
                .map(|c| c.to_der())
                .collect::<Result<_, _>>()?;
            hash.update(serde_json::to_vec(&chain)?);
            hash.update(cert.key.private_key_to_der()?);
        } else {
            hash.update(b"withdrawn");
        }
    }
    Ok(hash.finalize().into())
}

fn original_headers(route: &Route, backend: Option<&PreparedBackend>) -> Option<Vec<HeaderName>> {
    let settings = &route.settings;
    if route.script.is_some()
        || route.tenant.is_some()
        || route.rollout.is_some()
        || settings.gateway.is_some()
        || settings.security.jwt.is_some()
        || settings.security.external.is_some()
        || settings.compression.gzip > 0
        || settings.compression.brotli > 0
        || matches!(route.action, Action::Static)
        || backend.is_some_and(|b| {
            matches!(
                b.backend.options.balance,
                crate::backend::Balance::Sticky(_)
            )
        })
    {
        return None;
    }
    if settings
        .request_headers
        .iter()
        .any(|(_, v)| v.contains('$'))
        || settings
            .response_headers
            .iter()
            .any(|h| h.value.contains('$'))
        || matches!(&route.action, Action::Return { text, .. } if text.contains('$'))
    {
        return None;
    }
    let mut names = vec![http::header::CONTENT_TYPE];
    if settings.access_log.is_some() {
        names.extend([http::header::REFERER, http::header::USER_AGENT]);
    }
    let mut key = |key: &crate::traffic::Key| {
        let name = match key {
            crate::traffic::Key::Header(name) => name.parse().expect("validated header key"),
            crate::traffic::Key::Cookie(_) => http::header::COOKIE,
            _ => return,
        };
        if !names.contains(&name) {
            names.push(name);
        }
    };
    if let Some(rate) = &settings.traffic.rate {
        key(&rate.key);
    }
    if let Some(concurrency) = &settings.traffic.concurrency {
        key(&concurrency.key);
    }
    if let Some(backend) = backend
        && let crate::backend::Balance::Hash(hash) = &backend.backend.options.balance
    {
        key(hash);
    }
    Some(names)
}
