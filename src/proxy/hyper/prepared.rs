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
                routes.insert(
                    (host.listener, Arc::as_ptr(route) as usize),
                    PreparedRoute {
                        client,
                        request_headers,
                        response_headers,
                        backend,
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
