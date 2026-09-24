use crate::script::CompiledScript;
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Listener {
    pub address: SocketAddr,
    pub tls: bool,
    pub http2: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Endpoint {
    pub address: SocketAddr,
    pub weight: u32,
}

#[derive(Debug)]
pub struct Backend {
    pub endpoints: Vec<Endpoint>,
    pub tls: bool,
    pub hostname: String,
    pub host_header: String,
    cursor: AtomicU64,
    health: Mutex<Vec<EndpointHealth>>,
}
#[derive(Debug, Default)]
struct EndpointHealth {
    failures: usize,
    blocked_until: Option<Instant>,
}

impl Backend {
    pub fn new(endpoints: Vec<Endpoint>, tls: bool, hostname: String, host_header: String) -> Self {
        let health = Mutex::new(
            (0..endpoints.len())
                .map(|_| EndpointHealth::default())
                .collect(),
        );
        Self {
            endpoints,
            tls,
            hostname,
            host_header,
            cursor: AtomicU64::new(0),
            health,
        }
    }
    pub fn select(&self) -> Option<SocketAddr> {
        let now = Instant::now();
        let mut health = self.health.lock().unwrap_or_else(|e| e.into_inner());
        for state in health.iter_mut() {
            if state.blocked_until.is_some_and(|until| until <= now) {
                *state = EndpointHealth::default();
            }
        }
        let eligible = || {
            self.endpoints
                .iter()
                .zip(health.iter())
                .filter(|(_, h)| h.blocked_until.is_none())
                .map(|(e, _)| e)
        };
        let total: u64 = eligible().map(|e| u64::from(e.weight)).sum();
        if total == 0 {
            return None;
        }
        let mut n = self.cursor.fetch_add(1, Ordering::Relaxed) % total;
        for e in eligible() {
            if n < u64::from(e.weight) {
                return Some(e.address);
            }
            n -= u64::from(e.weight);
        }
        None
    }
    pub fn record_result(
        &self,
        address: SocketAddr,
        failed: bool,
        max_fails: usize,
        cooldown: Duration,
    ) -> bool {
        let Some(index) = self.endpoints.iter().position(|e| e.address == address) else {
            return false;
        };
        let mut health = self.health.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut health[index];
        if !failed {
            *state = EndpointHealth::default();
            return false;
        }
        if max_fails == 0 {
            return false;
        }
        state.failures = state.failures.saturating_add(1);
        if state.failures >= max_fails && state.blocked_until.is_none() {
            state.blocked_until = Some(Instant::now() + cooldown);
            return true;
        }
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum PathMatch {
    Exact(String),
    NginxPrefix(String),
    IngressPrefix(String),
    IngressDefault,
}

impl PathMatch {
    pub fn path(&self) -> &str {
        match self {
            Self::Exact(p) | Self::NginxPrefix(p) | Self::IngressPrefix(p) => p,
            Self::IngressDefault => "/",
        }
    }
    pub fn matches(&self, path: &str) -> bool {
        match self {
            Self::Exact(p) => path == p,
            Self::NginxPrefix(p) => path.starts_with(p),
            Self::IngressPrefix(p) => {
                let p = p.trim_end_matches('/');
                p.is_empty()
                    || path == p
                    || path.strip_prefix(p).is_some_and(|s| s.starts_with('/'))
            }
            Self::IngressDefault => true,
        }
    }
    pub fn rank(&self) -> (usize, bool) {
        match self {
            Self::IngressDefault => (0, false),
            _ => (self.path().len(), matches!(self, Self::Exact(_))),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub enum Action {
    Proxy {
        backend: String,
        uri: Option<String>,
    },
    Static,
    Return {
        status: u16,
        text: String,
    },
    Unavailable,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct AddedHeader {
    pub name: String,
    pub value: String,
    pub always: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Settings {
    pub root: PathBuf,
    pub index: Vec<String>,
    pub mime: BTreeMap<String, String>,
    pub default_type: String,
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<AddedHeader>,
    pub max_body: u64,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub keepalive: Duration,
    pub access_log: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            root: PathBuf::from("html"),
            index: vec!["index.html".into()],
            mime: BTreeMap::from([
                ("html".into(), "text/html".into()),
                ("css".into(), "text/css".into()),
                ("js".into(), "application/javascript".into()),
            ]),
            default_type: "application/octet-stream".into(),
            request_headers: vec![],
            response_headers: vec![],
            max_body: 1024 * 1024,
            connect_timeout: Duration::from_secs(60),
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(60),
            keepalive: Duration::from_secs(75),
            access_log: Some(PathBuf::from("/dev/stdout")),
        }
    }
}

pub struct Route {
    pub id: String,
    pub matcher: PathMatch,
    pub action: Action,
    pub settings: Settings,
    pub script: Option<Arc<CompiledScript>>,
    pub allowed_backends: BTreeMap<String, String>,
}

pub struct VirtualHost {
    pub listener: SocketAddr,
    pub names: Vec<String>,
    pub default: bool,
    pub ingress: bool,
    pub routes: Vec<Arc<Route>>,
}

pub struct Certificate {
    pub leaf: openssl::x509::X509,
    pub chain: Vec<openssl::x509::X509>,
    pub key: openssl::pkey::PKey<openssl::pkey::Private>,
}

impl Certificate {
    pub fn parse(cert: &[u8], key: &[u8]) -> Result<Self> {
        let mut chain = openssl::x509::X509::stack_from_pem(cert)?;
        if chain.is_empty() {
            bail!("empty certificate chain");
        }
        let leaf = chain.remove(0);
        let key = openssl::pkey::PKey::private_key_from_pem(key)?;
        if !leaf.public_key()?.public_eq(&key) {
            bail!("certificate and private key do not match");
        }
        Ok(Self { leaf, chain, key })
    }
}

pub struct TlsHost {
    pub listener: SocketAddr,
    pub name: String,
    pub ingress: bool,
    pub default: bool,
    // An unavailable certificate retains the SNI claim and prevents wildcard/default fallback.
    pub certificate: Option<Arc<Certificate>>,
}

pub struct RuntimeSnapshot {
    pub version: u64,
    pub ready: bool,
    pub content_hash: String,
    routing: crate::routing::Index,
    pub listeners: Vec<Listener>,
    pub hosts: Vec<VirtualHost>,
    pub backends: BTreeMap<String, Arc<Backend>>,
    pub certificates: Vec<TlsHost>,
    pub error_log: Option<Arc<crate::logging::ErrorOutput>>,
}

impl RuntimeSnapshot {
    pub fn empty(listeners: Vec<Listener>) -> Self {
        Self {
            version: 0,
            ready: true,
            content_hash: String::new(),
            routing: crate::routing::Index::default(),
            listeners,
            hosts: vec![],
            backends: BTreeMap::new(),
            certificates: vec![],
            error_log: None,
        }
    }
    pub fn fingerprint(&self) -> Result<String> {
        let certificates = self
            .certificates
            .iter()
            .map(|c| {
                let digest = if let Some(cert) = &c.certificate {
                    let mut certificate = Sha256::new();
                    certificate.update(cert.leaf.to_der()?);
                    for intermediate in &cert.chain {
                        certificate.update(intermediate.to_der()?);
                    }
                    Some(format!("{:x}", certificate.finalize()))
                } else {
                    None
                };
                Ok(serde_json::json!([
                    c.listener, c.name, c.ingress, c.default, digest,
                ]))
            })
            .collect::<Result<Vec<_>>>()?;
        let config = serde_json::json!({
            "listeners": self.listeners,
            "hosts": self.hosts.iter().map(|h| serde_json::json!([
                h.listener, h.names, h.default, h.ingress,
                h.routes.iter().map(|r| serde_json::json!([r.id, r.matcher, r.action, r.settings,
                    r.script.as_ref().map(|s| &s.digest), r.allowed_backends])).collect::<Vec<_>>()
            ])).collect::<Vec<_>>(),
            "backends": self.backends.iter().map(|(name, b)| serde_json::json!([name, b.endpoints, b.tls, b.hostname, b.host_header])).collect::<Vec<_>>(),
            "certificates": certificates,
        });
        Ok(format!(
            "{:x}",
            Sha256::digest(config.to_string().as_bytes())
        ))
    }
    pub fn reindex(&mut self) {
        self.routing = crate::routing::Index::build(&self.hosts);
    }
    pub fn route(&self, listener: SocketAddr, host: &str, path: &str) -> Option<Arc<Route>> {
        self.routing.route(&self.hosts, listener, host, path)
    }
    pub fn hostless_server_name(&self, listener: SocketAddr) -> Option<&str> {
        self.routing.hostless_server_name(&self.hosts, listener)
    }
    pub fn certificate(&self, listener: SocketAddr, host: &str) -> Option<Arc<Certificate>> {
        self.certificates
            .iter()
            .rev()
            .filter(|c| c.listener == listener)
            .filter_map(|c| {
                host_rank(&c.name, &host.to_ascii_lowercase(), c.ingress)
                    .or(if c.default { Some(0) } else { None })
                    .map(|rank| (rank, c))
            })
            .max_by_key(|(rank, _)| *rank)
            .and_then(|(_, c)| c.certificate.clone())
    }
}

fn host_rank(pattern: &str, host: &str, ingress: bool) -> Option<usize> {
    if pattern == host {
        return Some(usize::MAX);
    }
    if pattern.is_empty() && ingress {
        return Some(0);
    }
    let suffix = pattern.strip_prefix("*.")?;
    let prefix = host.strip_suffix(suffix)?.strip_suffix('.')?;
    if prefix.is_empty() || (ingress && prefix.contains('.')) {
        return None;
    }
    Some(suffix.len() + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefix_and_wildcard_boundaries_are_source_specific() {
        assert!(PathMatch::NginxPrefix("/api".into()).matches("/apix"));
        assert!(!PathMatch::IngressPrefix("/api".into()).matches("/apix"));
        assert!(PathMatch::IngressPrefix("/api/".into()).matches("/api/v1"));
        assert!(host_rank("*.example.org", "a.b.example.org", false).is_some());
        assert!(host_rank("*.example.org", "a.b.example.org", true).is_none());
    }
}
