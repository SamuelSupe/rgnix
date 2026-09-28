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
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

pub(crate) struct Prepared {
    pub pool_size: usize,
    // Route IDs need not be unique (exact and prefix locations can share a path).
    // These identities remain valid while the owning snapshot retains its routes.
    routes: HashMap<(SocketAddr, usize), PreparedRoute>,
}

pub(super) struct PreparedRoute {
    pub client: ProxyClient,
    pub request_headers: Vec<(HeaderName, Value)>,
    pub response_headers: Vec<(HeaderName, Value, bool)>,
    pub backend: Option<Arc<PreparedBackend>>,
    // Dynamic templates retain the complete original view; fixed routes capture
    // only fields read by policy and completion, before outbound header edits.
    pub original_headers: Option<Vec<HeaderName>>,
}

pub(super) struct PreparedBackend {
    pub backend: Arc<crate::backend::Backend>,
    pub host: HeaderValue,
    authorities: HashMap<SocketAddr, Authority>,
}

impl PreparedBackend {
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
    pub fn new(snapshot: &RuntimeSnapshot, pool_size: usize) -> Result<Self> {
        let mut clients = HashMap::new();
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
                    }),
                ))
            })
            .collect::<Result<_>>()?;
        let mut routes = HashMap::new();
        for host in &snapshot.hosts {
            for route in &host.routes {
                let settings = &route.settings;
                let key = (
                    host.listener,
                    settings.connect_timeout,
                    settings.read_timeout,
                    settings.write_timeout,
                    settings.keepalive,
                );
                let client = clients
                    .entry(key)
                    .or_insert_with(|| {
                        Client::builder(TokioExecutor::new())
                            .pool_timer(TokioTimer::new())
                            .pool_idle_timeout(settings.keepalive)
                            .pool_max_idle_per_host(pool_size)
                            .retry_canceled_requests(false)
                            .build(Connector::new(
                                settings.connect_timeout,
                                settings.read_timeout,
                                settings.write_timeout,
                            ))
                    })
                    .clone();
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
                routes.insert(
                    (host.listener, Arc::as_ptr(route) as usize),
                    PreparedRoute {
                        client,
                        request_headers,
                        response_headers,
                        backend,
                        original_headers,
                    },
                );
            }
        }
        Ok(Self { pool_size, routes })
    }

    pub(super) fn route(&self, listener: SocketAddr, route: &Arc<Route>) -> &PreparedRoute {
        &self.routes[&(listener, Arc::as_ptr(route) as usize)]
    }
}

fn original_headers(route: &Route, backend: Option<&PreparedBackend>) -> Option<Vec<HeaderName>> {
    let settings = &route.settings;
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
