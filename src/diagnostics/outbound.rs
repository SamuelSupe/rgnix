use super::*;
use crate::{
    model::{Action, Route},
    proxy::planning::{Variables, outbound_uri},
    script::Edits,
};

pub(super) struct Preview<'a> {
    pub snapshot: &'a RuntimeSnapshot,
    pub listener: SocketAddr,
    pub route: &'a Route,
    pub request: &'a RequestData,
    pub edits: &'a Edits,
    pub backend: Option<String>,
    pub original_uri: &'a str,
    pub original_peer: &'a str,
    pub scheme: &'a str,
    pub body_len: usize,
    pub sample: u64,
}
impl Preview<'_> {
    pub fn finish(self) -> Result<Value> {
        let variables = Variables {
            request: self.request,
            edits: self.edits,
            original_uri: self.original_uri,
            original_peer: self.original_peer,
            scheme: self.scheme,
            server_name: self
                .snapshot
                .hostless_server_name(self.listener)
                .unwrap_or(""),
        };
        let (backend, uri) = if let Some(backend) = self.backend {
            (
                self.route
                    .rollout
                    .as_ref()
                    .map_or_else(|| backend.clone(), |r| r.enforce(backend.clone())),
                None,
            )
        } else if let Some(policy) = &self.route.settings.gateway {
            if let Some((status, location)) =
                policy.location(self.request, &self.route.matcher, self.scheme == "https")
            {
                return Ok(json!({"kind":"redirect", "status":status, "location":location}));
            }
            let Some(backend) = self.route.rollout.as_ref().map_or_else(
                || policy.select_at(self.sample),
                |r| Some(r.select_at(self.request, self.sample)),
            ) else {
                return Ok(json!({"kind":"unavailable","status":500}));
            };
            (backend, None)
        } else {
            match &self.route.action {
                Action::Proxy { backend, uri } => (
                    self.route.rollout.as_ref().map_or_else(
                        || backend.clone(),
                        |r| r.select_at(self.request, self.sample),
                    ),
                    uri.as_deref(),
                ),
                Action::Return { status, text } => {
                    return Ok(
                        json!({"kind":"return","status":status,"value":variables.expand(text,"")}),
                    );
                }
                Action::Static => {
                    return Ok(
                        json!({"kind":"static","root":self.route.settings.root,"path":self.request.path}),
                    );
                }
                Action::Unavailable => return Ok(json!({"kind":"unavailable","status":503})),
            }
        };
        let mut edits = self.edits.clone();
        if edits.path.is_none()
            && let Some(path) = self
                .route
                .settings
                .gateway
                .as_ref()
                .and_then(|p| p.rewrite.as_ref())
                .and_then(|r| r.path.as_ref())
        {
            edits.path = Some(path.apply(&self.request.path, &self.route.matcher));
        }
        let target = outbound_uri(
            self.original_uri,
            self.request,
            &edits,
            &self.route.matcher,
            uri,
        );
        let _: http::Uri = target.parse()?;
        let upstream = self
            .snapshot
            .backends
            .get(&backend)
            .ok_or_else(|| anyhow::anyhow!("selected backend unavailable"))?;
        let mut headers = self.request.headers.clone();
        if let Some(connection) = headers.get("connection").cloned() {
            for token in connection.split(',') {
                headers.remove(&token.trim().to_ascii_lowercase());
            }
        }
        for name in [
            "connection",
            "keep-alive",
            "proxy-connection",
            "proxy-authenticate",
            "proxy-authorization",
            "upgrade",
            "te",
            "trailer",
        ] {
            headers.remove(name);
        }
        if self.request.headers.get("te").is_some_and(|v| {
            v.split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("trailers"))
        }) {
            headers.insert("te".into(), "trailers".into());
        }
        if let Some(policy) = &self.route.settings.gateway {
            let host = policy
                .rewrite
                .as_ref()
                .and_then(|r| r.hostname.as_ref())
                .unwrap_or(&self.request.host);
            headers.entry("host".into()).or_insert_with(|| host.clone());
            if policy
                .rewrite
                .as_ref()
                .is_some_and(|r| r.hostname.is_some())
            {
                headers.insert("host".into(), host.clone());
            }
            for name in &policy.request_headers.remove {
                headers.remove(&name.to_ascii_lowercase());
            }
            for h in &policy.request_headers.set {
                headers.insert(h.name.to_ascii_lowercase(), h.value.clone());
            }
            for h in &policy.request_headers.add {
                headers
                    .entry(h.name.to_ascii_lowercase())
                    .and_modify(|v| {
                        v.push_str(", ");
                        v.push_str(&h.value);
                    })
                    .or_insert_with(|| h.value.clone());
            }
        } else {
            headers.insert("host".into(), upstream.host_header.clone());
        }
        for (name, value) in &self.route.settings.request_headers {
            let value = variables.expand(value, &upstream.host_header);
            if value.is_empty() {
                headers.remove(&name.to_ascii_lowercase());
            } else {
                headers.insert(name.to_ascii_lowercase(), value);
            }
        }
        for (name, value) in &self.edits.headers {
            if let Some(value) = value {
                headers.insert(name.clone(), value.clone());
            } else {
                headers.remove(name);
            }
        }
        if self
            .request
            .headers
            .get("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            headers.insert("upgrade".into(), "websocket".into());
            headers.insert("connection".into(), "upgrade".into());
        }
        for (name, value) in &mut headers {
            if !matches!(
                name.as_str(),
                "host"
                    | "content-type"
                    | "content-length"
                    | "x-forwarded-for"
                    | "x-forwarded-proto"
                    | "x-real-ip"
                    | "te"
                    | "upgrade"
                    | "connection"
            ) {
                *value = "[redacted]".into();
            }
        }
        let mirror=self.route.rollout.as_ref().and_then(|r|r.policy.mirror.as_ref()).map(|m| {
            let protocol=self.request.headers.get("content-type").is_some_and(|v|v.starts_with("application/grpc")) || self.request.headers.get("upgrade").is_some_and(|v|v.eq_ignore_ascii_case("websocket"));
            let eligible=!protocol && self.body_len<=m.max_body_bytes;
            json!({"backend":m.service,"eligible":eligible,"selected":eligible && self.sample%100 < u64::from(m.percent),"percent":m.percent,"reason":if protocol {"streaming protocol"} else if self.body_len>m.max_body_bytes {"body exceeds mirror cap"} else {"sampling"}})
        }).or_else(|| self.route.settings.gateway.as_ref().and_then(|p| p.mirror.as_ref()).map(|m| {
            let protocol = self.request.headers.get("content-type").is_some_and(|v| v.starts_with("application/grpc")) || self.request.headers.contains_key("upgrade");
            let eligible = m.backend.is_some() && !protocol && self.body_len <= 64 * 1024;
            json!({"backend":m.backend,"eligible":eligible,"selected":eligible && self.sample % u64::from(m.denominator) < u64::from(m.numerator),"fraction":{"numerator":m.numerator,"denominator":m.denominator},"reason":if m.backend.is_none() {"unresolved mirror reference"} else if protocol {"streaming protocol"} else if self.body_len > 64 * 1024 {"body exceeds mirror cap"} else {"sampling"}})
        }));
        Ok(
            json!({"kind":"proxy","backend":backend,"uri":target,"headers":headers,"backend_state":upstream.diagnostic(),"traffic":self.route.rollout.as_ref().map(|r|r.diagnostic()),"mirror":mirror,"io_executed":false}),
        )
    }
}
