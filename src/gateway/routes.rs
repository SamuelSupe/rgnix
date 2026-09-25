use super::spec::*;
use crate::{
    model::{PathMatch, Route},
    script::RequestData,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{collections::BTreeSet, net::SocketAddr, sync::Arc};

#[derive(Clone, Default, Debug, Serialize)]
pub struct Policy {
    pub request_headers: Headers,
    pub response_headers: Headers,
    pub redirect: Option<Redirect>,
    pub rewrite: Option<Rewrite>,
    pub backends: Vec<(Option<String>, u32)>,
    pub listener_port: u16,
}
impl Policy {
    pub fn select(&self) -> Option<String> {
        let total: u64 = self.backends.iter().map(|(_, w)| u64::from(*w)).sum();
        if total == 0 {
            return None;
        }
        use rand::Rng;
        self.select_at(rand::thread_rng().gen_range(0..total))
    }
    pub(crate) fn select_at(&self, sample: u64) -> Option<String> {
        let total: u64 = self.backends.iter().map(|(_, w)| u64::from(*w)).sum();
        if total == 0 {
            return None;
        }
        let mut draw = sample % total;
        for (backend, weight) in &self.backends {
            if draw < u64::from(*weight) {
                return backend.clone();
            }
            draw -= u64::from(*weight);
        }
        None
    }
    pub fn location(
        &self,
        request: &RequestData,
        matcher: &PathMatch,
        tls: bool,
    ) -> Option<(u16, String)> {
        let redirect = self.redirect.as_ref()?;
        let scheme = redirect
            .scheme
            .as_deref()
            .unwrap_or(if tls { "https" } else { "http" });
        let host = redirect.hostname.as_deref().unwrap_or(&request.host);
        let port = redirect.port.unwrap_or_else(|| {
            if redirect.scheme.is_some() {
                if scheme == "https" { 443 } else { 80 }
            } else {
                self.listener_port
            }
        });
        let authority = if (scheme == "https" && port == 443) || (scheme == "http" && port == 80) {
            host.to_owned()
        } else {
            format!("{host}:{port}")
        };
        let path = redirect
            .path
            .as_ref()
            .map_or_else(|| request.path.clone(), |p| p.apply(&request.path, matcher));
        let query = if request.query.is_empty() {
            String::new()
        } else {
            format!("?{}", request.query)
        };
        Some((
            redirect.status_code.unwrap_or(302),
            format!(
                "{scheme}://{authority}{}{query}",
                crate::proxy::encode_path(&path)
            ),
        ))
    }
}

impl Headers {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.set.len() + self.add.len() + self.remove.len() <= 64,
            "too many header operations"
        );
        let mut names = BTreeSet::new();
        for name in self
            .set
            .iter()
            .chain(&self.add)
            .map(|h| &h.name)
            .chain(&self.remove)
        {
            http::header::HeaderName::from_bytes(name.as_bytes())?;
            let name = name.to_ascii_lowercase();
            ensure!(
                ![
                    "host",
                    "content-length",
                    "transfer-encoding",
                    "connection",
                    "upgrade",
                    "te",
                    "trailer",
                    "keep-alive",
                    "proxy-connection"
                ]
                .contains(&name.as_str()),
                "header {name} is transport-managed"
            );
            ensure!(names.insert(name), "duplicate header operation");
        }
        for header in self.set.iter().chain(&self.add) {
            ensure!(header.value.len() <= 8192, "header value exceeds 8 KiB");
            http::HeaderValue::from_str(&header.value)?;
        }
        Ok(())
    }
    pub fn request(&self, headers: &mut pingora::http::RequestHeader) -> pingora::Result<()> {
        for name in &self.remove {
            headers.remove_header(name);
        }
        for h in &self.set {
            headers.insert_header(h.name.clone(), h.value.clone())?;
        }
        for h in &self.add {
            headers.append_header(h.name.clone(), h.value.clone())?;
        }
        Ok(())
    }
    pub fn response(&self, headers: &mut pingora::http::ResponseHeader) -> pingora::Result<()> {
        for name in &self.remove {
            headers.remove_header(name);
        }
        for h in &self.set {
            headers.insert_header(h.name.clone(), h.value.clone())?;
        }
        for h in &self.add {
            headers.append_header(h.name.clone(), h.value.clone())?;
        }
        Ok(())
    }
}
impl PathModifier {
    pub fn validate(&self, prefix: bool) -> Result<()> {
        let path = match self.type_.as_str() {
            "ReplaceFullPath" if self.replace_prefix_match.is_none() => {
                self.replace_full_path.as_deref()
            }
            "ReplacePrefixMatch" if prefix && self.replace_full_path.is_none() => {
                self.replace_prefix_match.as_deref()
            }
            _ => None,
        }
        .context("invalid path modifier or ReplacePrefixMatch without PathPrefix")?;
        ensure!(
            path.starts_with('/') && !path.contains(['?', '#', '\r', '\n']) && path.len() <= 1024,
            "invalid replacement path"
        );
        Ok(())
    }
    pub fn apply(&self, path: &str, matcher: &PathMatch) -> String {
        if let Some(full) = &self.replace_full_path {
            return full.clone();
        }
        let prefix = self.replace_prefix_match.as_deref().unwrap_or("/");
        let suffix = path
            .strip_prefix(matcher.path().trim_end_matches('/'))
            .unwrap_or("");
        if suffix.is_empty() {
            return prefix.to_owned();
        }
        format!(
            "{}/{}",
            prefix.trim_end_matches('/'),
            suffix.trim_start_matches('/')
        )
    }
}

pub fn filters(rule: &Rule, port: u16, prefix: bool) -> Result<Policy> {
    let mut policy = Policy {
        listener_port: port,
        ..Default::default()
    };
    let mut seen = BTreeSet::new();
    for filter in &rule.filters {
        ensure!(seen.insert(&filter.type_), "repeated filter type");
        ensure!(
            usize::from(filter.request_header_modifier.is_some())
                + usize::from(filter.response_header_modifier.is_some())
                + usize::from(filter.request_redirect.is_some())
                + usize::from(filter.url_rewrite.is_some())
                == 1,
            "filter must contain exactly one configuration"
        );
        match filter.type_.as_str() {
            "RequestHeaderModifier" => {
                policy.request_headers = filter
                    .request_header_modifier
                    .clone()
                    .context("missing requestHeaderModifier")?
            }
            "ResponseHeaderModifier" => {
                policy.response_headers = filter
                    .response_header_modifier
                    .clone()
                    .context("missing responseHeaderModifier")?
            }
            "RequestRedirect" => {
                policy.redirect = Some(
                    filter
                        .request_redirect
                        .clone()
                        .context("missing requestRedirect")?,
                )
            }
            "URLRewrite" => {
                policy.rewrite = Some(filter.url_rewrite.clone().context("missing urlRewrite")?)
            }
            _ => anyhow::bail!("unsupported filter {}", filter.type_),
        }
    }
    ensure!(
        policy.redirect.is_none() || policy.rewrite.is_none(),
        "RequestRedirect and URLRewrite are incompatible"
    );
    policy.request_headers.validate()?;
    policy.response_headers.validate()?;
    for path in policy
        .redirect
        .iter()
        .filter_map(|r| r.path.as_ref())
        .chain(policy.rewrite.iter().filter_map(|r| r.path.as_ref()))
    {
        path.validate(prefix)?;
    }
    for host in policy
        .redirect
        .iter()
        .filter_map(|r| r.hostname.as_deref())
        .chain(policy.rewrite.iter().filter_map(|r| r.hostname.as_deref()))
    {
        ensure!(
            !host.starts_with("*.") && valid_hostname(host),
            "invalid replacement hostname"
        );
    }
    if let Some(r) = &policy.redirect {
        ensure!(
            r.scheme
                .as_ref()
                .is_none_or(|s| s == "http" || s == "https"),
            "invalid redirect scheme"
        );
        ensure!(
            [301, 302, 303, 307, 308].contains(&r.status_code.unwrap_or(302)),
            "invalid redirect status"
        );
        ensure!(r.port != Some(0), "invalid redirect port");
    }
    Ok(policy)
}

#[derive(Clone, Default, Serialize)]
pub struct Routing {
    pub listeners: Vec<ListenerRoutes>,
}
#[derive(Clone, Serialize)]
pub struct ListenerRoutes {
    pub address: SocketAddr,
    pub hostname: String,
    pub entries: Vec<Entry>,
}
#[derive(Clone, Serialize)]
pub struct Entry {
    pub hostname: String,
    pub matcher: Match,
    pub grpc: bool,
    pub order: (String, String, String, usize, usize),
    pub route_id: String,
    #[serde(skip)]
    pub route: Arc<Route>,
}

pub fn valid_hostname(host: &str) -> bool {
    let name = host.strip_prefix("*.").unwrap_or(host);
    !name.is_empty() && name.len() <= 253 && name.split('.').all(crate::tenancy::namespace_name)
}
pub fn host_rank(pattern: &str, host: &str) -> Option<usize> {
    if pattern.is_empty() {
        return Some(0);
    }
    if pattern == host {
        return Some(usize::MAX);
    }
    let suffix = pattern.strip_prefix("*.")?;
    host.strip_suffix(suffix)?
        .strip_suffix('.')
        .filter(|p| !p.is_empty())?;
    Some(suffix.len())
}
pub fn intersect(left: &str, right: &str) -> Option<String> {
    if left.is_empty() {
        return Some(right.into());
    }
    if right.is_empty() || host_rank(right, left).is_some() {
        return Some(left.into());
    }
    host_rank(left, right).map(|_| right.into())
}
impl Match {
    pub fn path_match(&self, grpc: bool) -> Result<PathMatch> {
        ensure!(
            self.headers.len() <= 16 && self.query_params.len() <= 16,
            "too many match predicates"
        );
        for m in self.headers.iter().chain(&self.query_params) {
            ensure!(
                m.type_ == "Exact",
                "only Exact header/query matches are supported"
            );
        }
        for m in &self.headers {
            http::header::HeaderName::from_bytes(m.name.as_bytes())?;
        }
        if grpc {
            ensure!(
                self.path.is_none() && self.query_params.is_empty(),
                "invalid GRPCRoute match"
            );
            if let Some(method) = &self.method {
                let object = method.as_object().context("invalid gRPC method match")?;
                ensure!(
                    object
                        .keys()
                        .all(|k| ["type", "service", "method"].contains(&k.as_str())),
                    "unknown gRPC match field"
                );
                ensure!(
                    method
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Exact")
                        == "Exact",
                    "gRPC regular expressions are unsupported"
                );
                for key in ["service", "method"] {
                    if let Some(value) = object.get(key) {
                        ensure!(
                            value
                                .as_str()
                                .is_some_and(|s| !s.is_empty() && !s.contains('/')),
                            "invalid gRPC method component"
                        );
                    }
                }
            }
            return Ok(PathMatch::IngressPrefix("/".into()));
        }
        if let Some(method) = &self.method {
            http::Method::from_bytes(method.as_str().context("invalid HTTP method")?.as_bytes())?;
        }
        let Some(path) = &self.path else {
            return Ok(PathMatch::IngressPrefix("/".into()));
        };
        ensure!(
            path.value.starts_with('/')
                && !path.value.contains(['?', '#'])
                && path.value.len() <= 1024,
            "invalid path match"
        );
        match path.type_.as_str() {
            "Exact" => Ok(PathMatch::Exact(path.value.clone())),
            "PathPrefix" => Ok(PathMatch::IngressPrefix(path.value.clone())),
            _ => anyhow::bail!("regular expression paths are unsupported"),
        }
    }
    fn matches(&self, entry: &Entry, request: &RequestData) -> bool {
        if !entry.route.matcher.matches(&request.path) {
            return false;
        }
        if entry.grpc {
            if request.method != "POST"
                || !request.headers.get("content-type").is_some_and(|v| {
                    v.split(';').next().is_some_and(|t| {
                        t == "application/grpc" || t.starts_with("application/grpc+")
                    })
                })
            {
                return false;
            }
            let Some((service, method)) = request
                .path
                .strip_prefix('/')
                .and_then(|p| p.split_once('/'))
            else {
                return false;
            };
            if let Some(m) = &self.method
                && (m
                    .get("service")
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| v != service)
                    || m.get("method")
                        .and_then(|v| v.as_str())
                        .is_some_and(|v| v != method))
            {
                return false;
            }
        } else if self
            .method
            .as_ref()
            .and_then(|m| m.as_str())
            .is_some_and(|m| m != request.method)
        {
            return false;
        }
        if self
            .headers
            .iter()
            .any(|h| request.headers.get(&h.name.to_ascii_lowercase()) != Some(&h.value))
        {
            return false;
        }
        let query: Vec<_> = url::form_urlencoded::parse(request.query.as_bytes()).collect();
        self.query_params.iter().all(|q| {
            query
                .iter()
                .find(|(name, _)| name == &q.name)
                .is_some_and(|(_, value)| value == &q.value)
        })
    }
}
impl Entry {
    fn rank(&self, host: &str) -> Option<(usize, bool, usize, usize, usize, usize)> {
        let (length, exact) = self.route.matcher.rank();
        let method = if self.grpc {
            self.matcher.method.as_ref().map_or(0, |m| {
                usize::from(m.get("service").is_some()) + usize::from(m.get("method").is_some())
            })
        } else {
            usize::from(self.matcher.method.is_some())
        };
        Some((
            host_rank(&self.hostname, host)?,
            exact,
            length,
            method,
            self.matcher.headers.len(),
            self.matcher.query_params.len(),
        ))
    }
}
impl Routing {
    pub fn route(&self, address: SocketAddr, request: &RequestData) -> Option<Arc<Route>> {
        let host = request.host.to_ascii_lowercase();
        let listener = self
            .listeners
            .iter()
            .filter(|l| l.address == address)
            .filter_map(|l| host_rank(&l.hostname, &host).map(|rank| (rank, l)))
            .max_by_key(|(rank, _)| *rank)?
            .1;
        listener
            .entries
            .iter()
            .filter(|e| e.matcher.matches(e, request))
            .filter_map(|e| e.rank(&host).map(|rank| (rank, e)))
            .max_by(|(rank_a, a), (rank_b, b)| {
                rank_a.cmp(rank_b).then_with(|| b.order.cmp(&a.order))
            })
            .map(|(_, e)| e.route.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Action, Settings};
    use serde_json::json;
    fn entry(id: &str, hostname: &str, matcher: serde_json::Value) -> Entry {
        let matcher: Match = serde_json::from_value(matcher).unwrap();
        let path = matcher.path_match(false).unwrap();
        Entry {
            hostname: hostname.into(),
            matcher,
            grpc: false,
            order: ("2026-01-01".into(), "test".into(), id.into(), 0, 0),
            route_id: id.into(),
            route: Arc::new(Route {
                id: id.into(),
                tenant: None,
                rollout: None,
                matcher: path,
                action: Action::Unavailable,
                settings: Settings::default(),
                script: None,
                allowed_backends: Default::default(),
            }),
        }
    }
    #[test]
    fn request_predicates_and_listener_isolation() {
        let address = "127.0.0.1:8080".parse().unwrap();
        let mut routing = Routing {
            listeners: vec![
                ListenerRoutes {
                    address,
                    hostname: "*.example.test".into(),
                    entries: vec![entry("wildcard", "", json!({}))],
                },
                ListenerRoutes {
                    address,
                    hostname: "api.example.test".into(),
                    entries: vec![
                        entry("prefix", "", json!({"path":{"value":"/api"}})),
                        entry(
                            "predicate",
                            "",
                            json!({"path":{"type":"Exact","value":"/api"},"method":"POST","headers":[{"name":"X-Canary","value":"1"}],"queryParams":[{"name":"v","value":"2"}]}),
                        ),
                    ],
                },
            ],
        };
        let mut request = RequestData {
            host: "api.example.test".into(),
            path: "/api".into(),
            method: "POST".into(),
            query: "v=2&v=3".into(),
            headers: [("x-canary".into(), "1".into())].into(),
            ..Default::default()
        };
        assert_eq!(routing.route(address, &request).unwrap().id, "predicate");
        request.query = "v=3&v=2".into();
        assert_eq!(routing.route(address, &request).unwrap().id, "prefix");
        request.path = "/apix".into();
        assert!(routing.route(address, &request).is_none());
        request.host = "a.b.example.test".into();
        assert_eq!(routing.route(address, &request).unwrap().id, "wildcard");
        routing.listeners[1].entries.clear();
        request.host = "api.example.test".into();
        assert!(routing.route(address, &request).is_none());
    }
    #[test]
    fn invalid_backends_keep_their_weight_and_path_rewrite_preserves_boundaries() {
        let policy = Policy {
            backends: vec![
                (Some("valid".into()), 9),
                (None, 1),
                (Some("disabled".into()), 0),
            ],
            ..Default::default()
        };
        assert_eq!(
            (0..100)
                .filter(|sample| policy.select_at(*sample).is_none())
                .count(),
            10
        );
        let modifier = PathModifier {
            type_: "ReplacePrefixMatch".into(),
            replace_prefix_match: Some("/new/".into()),
            replace_full_path: None,
        };
        modifier.validate(true).unwrap();
        assert!(modifier.validate(false).is_err());
        assert_eq!(
            modifier.apply("/old/a", &PathMatch::IngressPrefix("/old/".into())),
            "/new/a"
        );
        assert_eq!(
            modifier.apply("/old", &PathMatch::IngressPrefix("/old/".into())),
            "/new/"
        );
    }
}
