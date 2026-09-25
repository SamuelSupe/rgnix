use crate::script::CompiledScript;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Listener {
    pub address: SocketAddr,
    pub tls: bool,
    pub http2: bool,
    pub proxy_protocol: bool,
    pub proxy_trusted: Vec<ipnet::IpNet>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Endpoint {
    pub address: SocketAddr,
    pub weight: u32,
}

pub use crate::backend::Backend;

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
    pub gateway: Option<crate::gateway::Policy>,
    pub root: PathBuf,
    pub index: Vec<String>,
    pub mime: BTreeMap<String, String>,
    pub default_type: String,
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<AddedHeader>,
    pub max_body: u64,
    pub body_policy: crate::body::BodyPolicy,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub keepalive: Duration,
    pub access_log: Option<PathBuf>,
    pub log_policy: crate::logging::access::Policy,
    pub identity: crate::identity::IdentityPolicy,
    pub traffic: crate::traffic::Policy,
    pub upstream: crate::upstream::Transport,
    pub compression: crate::compression::Compression,
    pub alias: Option<PathBuf>,
    pub try_files: Vec<String>,
    pub security: crate::security::Security,
    pub https_redirect_port: Option<u16>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            gateway: None,
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
            body_policy: crate::body::BodyPolicy::default(),
            connect_timeout: Duration::from_secs(60),
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(60),
            keepalive: Duration::from_secs(75),
            access_log: Some(PathBuf::from("/dev/stdout")),
            log_policy: Default::default(),
            identity: Default::default(),
            traffic: Default::default(),
            upstream: Default::default(),
            compression: Default::default(),
            alias: None,
            try_files: vec![],
            security: Default::default(),
            https_redirect_port: None,
        }
    }
}

#[derive(Clone)]
pub struct Route {
    pub id: String,
    pub tenant: Option<Arc<crate::tenancy::Tenant>>,
    pub rollout: Option<Arc<crate::rollout::State>>,
    pub matcher: PathMatch,
    pub action: Action,
    pub settings: Settings,
    pub script: Option<Arc<CompiledScript>>,
    pub allowed_backends: BTreeMap<String, String>,
}

#[derive(Clone)]
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

#[derive(Clone)]
pub struct TlsHost {
    pub listener: SocketAddr,
    pub name: String,
    pub ingress: bool,
    pub default: bool,
    // An unavailable certificate retains the SNI claim and prevents wildcard/default fallback.
    pub certificate: Option<Arc<Certificate>>,
    pub client_auth: crate::security::mtls::Policy,
}

#[derive(Clone)]
pub struct RuntimeSnapshot {
    pub gateway: Option<crate::gateway::Routing>,
    pub version: u64,
    pub source_bundle: Option<Arc<crate::config::bundle::Bundle>>,
    pub ready: bool,
    pub content_hash: String,
    routing: crate::routing::Index,
    pub listeners: Vec<Listener>,
    pub hosts: Vec<VirtualHost>,
    pub backends: BTreeMap<String, Arc<Backend>>,
    pub certificates: Vec<TlsHost>,
    pub error_log: Option<Arc<crate::logging::ErrorOutput>>,
    pub log_rotation: crate::logging::Rotation,
    pub default_access_log: Option<PathBuf>,
}

impl RuntimeSnapshot {
    pub fn empty(listeners: Vec<Listener>) -> Self {
        Self {
            gateway: None,
            version: 0,
            source_bundle: None,
            ready: true,
            content_hash: String::new(),
            routing: crate::routing::Index::default(),
            listeners,
            hosts: vec![],
            backends: BTreeMap::new(),
            certificates: vec![],
            error_log: None,
            log_rotation: crate::logging::Rotation::default(),
            default_access_log: Some("/dev/stdout".into()),
        }
    }
    pub fn fingerprint(&self) -> Result<String> {
        if let Some(bundle) = &self.source_bundle {
            return Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(bundle)?)));
        }
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
                    c.listener,
                    c.name,
                    c.ingress,
                    c.default,
                    digest,
                    c.client_auth,
                ]))
            })
            .collect::<Result<Vec<_>>>()?;
        let config = serde_json::json!({
            "gateway": self.gateway,
            "listeners": self.listeners,
            "hosts": self.hosts.iter().map(|h| serde_json::json!([
                h.listener, h.names, h.default, h.ingress,
                h.routes.iter().map(|r| serde_json::json!([r.id, r.matcher, r.action, r.settings,
                    r.script.as_ref().map(|s| &s.digest), r.allowed_backends,
                    r.rollout.as_ref().map(|rollout|&rollout.policy), r.tenant.as_ref().map(|tenant|tenant.quota.load_full())])).collect::<Vec<_>>()
            ])).collect::<Vec<_>>(),
            "backends": self.backends.iter().map(|(name, b)| serde_json::json!([name, b.endpoints, b.tls, b.hostname, b.host_header, b.options, b.origins, b.ca_pem, b.profile])).collect::<Vec<_>>(),
            "certificates": certificates,
            "log_rotation": self.log_rotation,
            "default_access_log": self.default_access_log,
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
        if let Some(gateway) = &self.gateway {
            return gateway.route(
                listener,
                &crate::script::RequestData {
                    host: host.into(),
                    path: path.into(),
                    method: "GET".into(),
                    ..Default::default()
                },
            );
        }
        self.routing.route(&self.hosts, listener, host, path)
    }
    pub fn route_request(
        &self,
        listener: SocketAddr,
        request: &crate::script::RequestData,
    ) -> Option<Arc<Route>> {
        if let Some(gateway) = &self.gateway {
            gateway.route(listener, request)
        } else {
            self.route(listener, &request.host, &request.path)
        }
    }
    pub fn hostless_server_name(&self, listener: SocketAddr) -> Option<&str> {
        self.routing.hostless_server_name(&self.hosts, listener)
    }
    pub fn certificate(&self, listener: SocketAddr, host: &str) -> Option<Arc<Certificate>> {
        self.tls_host(listener, host)
            .and_then(|c| c.certificate.clone())
    }
    pub fn tls_host(&self, listener: SocketAddr, host: &str) -> Option<&TlsHost> {
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
            .map(|(_, c)| c)
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
