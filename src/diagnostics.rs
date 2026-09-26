pub(crate) mod auth;
mod outbound;
pub mod preflight;
mod simulation;
use crate::{
    model::RuntimeSnapshot,
    runtime::Shared,
    script::{Decision, RequestData},
};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use pingora::{apps::http_app::ServeHttp, protocols::http::ServerSession};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
pub use simulation::{Simulation, simulate};
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Clone, clap::Args)]
#[group(id = "diagnostics")]
pub struct Options {
    /// Enables protected /v1 diagnostics and rollback endpoints. File contains a bearer token.
    #[arg(long, env = "RGNIX_ADMIN_TOKEN_FILE")]
    pub admin_token_file: Option<PathBuf>,
    /// Read-only token for diagnostics and offline simulations.
    #[arg(long, env = "RGNIX_ADMIN_READ_TOKEN_FILE")]
    pub admin_read_token_file: Option<PathBuf>,
    /// Named identities with roles and namespace scopes; JSON, automatically reloaded.
    #[arg(long)]
    pub admin_users_file: Option<PathBuf>,
    /// JSON audit output; defaults to stderr when diagnostics are enabled.
    #[arg(long)]
    pub admin_audit_file: Option<PathBuf>,
    /// Private persistent configuration history, standalone mode only.
    #[arg(long)]
    pub history_dir: Option<PathBuf>,
    /// TLS AdmissionReview listener; requires certificate and key in Ingress/Gateway mode.
    #[arg(long)]
    pub admission_listen: Option<SocketAddr>,
    #[arg(long)]
    pub admission_tls_cert: Option<PathBuf>,
    #[arg(long)]
    pub admission_tls_key: Option<PathBuf>,
    /// Kubernetes service-account identity permitted to update controller rollout metadata.
    #[arg(long)]
    pub admission_controller_user: Option<String>,
}
impl Options {
    pub fn token_hash(path: &Option<PathBuf>) -> Result<Option<[u8; 32]>> {
        path.as_ref()
            .map(|path| {
                let value = String::from_utf8(crate::controls::read_bounded(path, 4098)?)?;
                let token = value.trim();
                ensure!(
                    (32..=4096).contains(&token.len())
                        && token.bytes().all(|b| b.is_ascii_graphic()),
                    "admin token must be 32..4096 printable bytes"
                );
                Ok(Sha256::digest(token.as_bytes()).into())
            })
            .transpose()
    }
}
pub struct Admin {
    pub shared: Arc<Shared>,
    pub audit: Arc<crate::audit::Audit>,
}
#[async_trait]
impl ServeHttp for Admin {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        session.set_keepalive(None);
        let path = session.req_header().uri.path();
        let (status, body, content_type) = if !path.starts_with("/v1/") {
            self.shared.telemetry.render(path)
        } else {
            let principal = self
                .shared
                .controls
                .active
                .load()
                .credentials
                .authenticate(&session.req_header().headers);
            let actor = principal
                .as_ref()
                .map_or("unauthenticated", |p| p.name.as_str());
            let operation = if path.starts_with("/v1/rollback/") {
                "rollback"
            } else {
                "diagnostics"
            };
            let audit_operation = if operation == "rollback" {
                format!(
                    "rollback/{}",
                    path.strip_prefix("/v1/rollback/")
                        .and_then(|v| v.parse::<u64>().ok())
                        .map_or_else(|| "invalid".into(), |v| v.to_string())
                )
            } else {
                match path {
                    "/v1/config"
                    | "/v1/fleet"
                    | "/v1/routes"
                    | "/v1/backends"
                    | "/v1/history"
                    | "/v1/explain"
                    | "/v1/simulate"
                    | "/v1/rollouts"
                    | "/v1/validate"
                    | "/v1/validate-ingress" => path,
                    "/v1/validate-gateway" => path,
                    _ => "unknown",
                }
                .to_string()
            };
            let version = self.shared.snapshot.load().version;
            let result = match principal.as_ref() {
                None => (
                    401,
                    json!({"error":"diagnostics require a configured bearer token"}),
                ),
                Some(p) if operation == "rollback" && !(p.writer() && p.global()) => {
                    (403, json!({"error":"rollback requires a global writer"}))
                }
                Some(_)
                    if self
                        .audit
                        .record(actor, &audit_operation, version, "attempt")
                        .is_err() =>
                {
                    (503, json!({"error":"audit output unavailable"}))
                }
                Some(p) => self.handle(session, p).await,
            };
            if let Err(e) = self.audit.record(
                actor,
                &audit_operation,
                self.shared.snapshot.load().version,
                &result.0.to_string(),
            ) {
                log::error!("admin audit: {e}");
            }
            let body = serde_json::to_vec(&result.1).unwrap_or_default();
            if body.len() > 8 * 1024 * 1024 {
                (
                    413,
                    b"{\"error\":\"diagnostic response exceeds 8 MiB\"}".to_vec(),
                    "application/json",
                )
            } else {
                (result.0, body, "application/json")
            }
        };
        http::Response::builder()
            .status(status)
            .header("Content-Type", content_type)
            .header("Cache-Control", "no-store")
            .body(body)
            .unwrap()
    }
}
impl Admin {
    async fn handle(
        &self,
        session: &mut ServerSession,
        principal: &auth::Principal,
    ) -> (u16, Value) {
        if matches!(
            session.req_header().uri.path(),
            "/v1/validate" | "/v1/validate-ingress" | "/v1/validate-gateway"
        ) {
            return self.validate(session, principal).await;
        }
        if session.req_header().uri.path() == "/v1/rollouts" {
            if !principal.writer() {
                return (403, json!({"error":"rollout operations require a writer"}));
            }
            let command: crate::ingress::release::Command = match read_json(session).await {
                Ok(value) => value,
                Err(result) => return result,
            };
            let Some((namespace, _)) = command.owner.split_once('/') else {
                return (400, json!({"error":"owner requires namespace/name"}));
            };
            if !principal.allows(namespace) {
                return (403, json!({"error":"rollout outside identity scope"}));
            }
            let operation = format!(
                "rollout/{}/{}/{:?}",
                command.owner, command.revision, command.operation
            );
            if self
                .audit
                .record(
                    &principal.name,
                    &operation,
                    self.shared.snapshot.load().version,
                    "attempt",
                )
                .is_err()
            {
                return (503, json!({"error":"audit output unavailable"}));
            }
            return match crate::ingress::release::command(&self.shared, command).await {
                Ok(()) => (
                    200,
                    json!({"accepted":true,"publication":"Kubernetes watch"}),
                ),
                Err(e) => (409, json!({"error":e.to_string()})),
            };
        }
        if session.req_header().uri.path() == "/v1/simulate" {
            let Ok(_permit) = self.shared.simulations.try_acquire() else {
                return (429, json!({"error":"simulation budget exhausted"}));
            };
            if session.req_header().method != http::Method::POST {
                return (405, json!({"error":"simulation requires POST"}));
            }
            session.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut bytes = Vec::new();
            loop {
                match session.read_request_body().await {
                    Ok(Some(chunk)) if bytes.len() + chunk.len() <= 1024 * 1024 => {
                        bytes.extend_from_slice(&chunk)
                    }
                    Ok(None) => break,
                    _ => {
                        return (
                            413,
                            json!({"error":"simulation body unavailable or exceeds 1 MiB"}),
                        );
                    }
                }
            }
            let input: Simulation = match serde_json::from_slice(&bytes) {
                Ok(input) => input,
                Err(e) => return (400, json!({"error":e.to_string()})),
            };
            let snapshot = self.shared.snapshot.load_full();
            if !principal.global() {
                let selected = input
                    .listener
                    .or_else(|| snapshot.listeners.first().map(|l| l.address))
                    .and_then(|listener| {
                        input
                            .routing_request()
                            .ok()
                            .and_then(|request| snapshot.route_request(listener, &request))
                    });
                if selected
                    .as_ref()
                    .is_none_or(|route| !principal.route(route))
                {
                    return (403, json!({"error":"route is outside identity scope"}));
                }
            }
            let result = tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(simulate(&snapshot, input, false))
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r);
            return match result {
                Ok(value) => (200, value),
                Err(e) => (400, json!({"error":e.to_string()})),
            };
        }
        let uri = &session.req_header().uri;
        let path = uri.path();
        if let Some(version) = path.strip_prefix("/v1/rollback/") {
            if session.req_header().method != http::Method::POST {
                return (405, json!({"error":"rollback requires POST"}));
            }
            let result = match version.parse::<u64>() {
                Ok(version) => {
                    let shared = self.shared.clone();
                    tokio::task::spawn_blocking(move || shared.rollback(version))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|r| r)
                }
                Err(e) => Err(e.into()),
            };
            return match result {
                Ok(()) => (200, json!({"version":self.shared.snapshot.load().version})),
                Err(e) => (409, json!({"error":e.to_string()})),
            };
        }
        if session.req_header().method != http::Method::GET {
            return (405, json!({"error":"diagnostics require GET"}));
        }
        let current = self.shared.snapshot.load_full();
        let snapshot = principal.view(&current);
        let data = match path {
            "/v1/fleet" if principal.global() => self.shared.fleet.diagnostic(),
            "/v1/fleet" => return (403, json!({"error":"replica status requires global scope"})),
            "/v1/config" => {
                let mut value = describe(&snapshot);
                value["controls_sha256"] = json!(self.shared.controls.active.load().digest);
                value["shared_rate_limit"] = self
                    .shared
                    .controls
                    .active
                    .load()
                    .global_rate
                    .as_ref()
                    .map_or_else(|| json!({"enabled":false}), |limiter| limiter.diagnostic());
                value["principal"] = json!({"name":principal.name,"role":principal.role,"namespaces":principal.namespaces});
                let gates: std::collections::BTreeSet<_> = snapshot
                    .hosts
                    .iter()
                    .flat_map(|h| &h.routes)
                    .filter_map(|r| r.rollout.as_ref())
                    .flat_map(|r| r.policy.metric_gates.iter().map(String::as_str))
                    .collect();
                value["metric_gates"] = crate::rollout::metrics::diagnostic(&self.shared, gates);
                value
            }
            "/v1/routes" => routes(&snapshot),
            "/v1/backends" => backends(&snapshot),
            "/v1/history" if principal.global() => self.shared.history(),
            "/v1/history" => return (403, json!({"error":"history requires global scope"})),
            "/v1/explain" => {
                let args: BTreeMap<_, _> =
                    url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
                        .into_owned()
                        .collect();
                let Some(listener) = args
                    .get("listener")
                    .and_then(|v| v.parse().ok())
                    .or_else(|| snapshot.listeners.first().map(|l| l.address))
                else {
                    return (400, json!({"error":"missing listener"}));
                };
                let host = args.get("host").map_or("", String::as_str);
                let path = args.get("path").map_or("/", String::as_str);
                if !principal.global()
                    && crate::proxy::normalized_path(path.split('?').next().unwrap_or("/"))
                        .ok()
                        .and_then(|path| current.route(listener, host, &path))
                        .as_ref()
                        .is_none_or(|r| !principal.route(r))
                {
                    return (403, json!({"error":"route is outside identity scope"}));
                }
                match explain(
                    &snapshot,
                    listener,
                    args.get("host").map_or("", String::as_str),
                    args.get("path").map_or("/", String::as_str),
                ) {
                    Ok(value) => value,
                    Err(e) => return (400, json!({"error":e.to_string()})),
                }
            }
            _ => return (404, json!({"error":"not found"})),
        };
        (200, data)
    }
}
pub fn describe(snapshot: &RuntimeSnapshot) -> Value {
    json!({"version":snapshot.version,"sha256":snapshot.content_hash,"ready":snapshot.ready,"listeners":snapshot.listeners,"routes":routes(snapshot),"backends":backends(snapshot),"certificates":snapshot.certificates.iter().map(|c|json!({"listener":c.listener,"name":c.name,"available":c.certificate.as_ref().is_some_and(|cert| cert.valid_time().is_ok()),"health":c.certificate.as_ref().map(|cert| cert.diagnostic(&c.name))})).collect::<Vec<_>>()})
}
fn route(route: &crate::model::Route) -> Value {
    let mut settings = serde_json::to_value(&route.settings).unwrap();
    for pointer in ["/security/external/url", "/security/jwt/spec/source"] {
        if let Some(value) = settings.pointer_mut(pointer)
            && let Some(text) = value.as_str()
            && let Ok(mut url) = url::Url::parse(text)
        {
            url.set_query(None);
            *value = Value::String(url.to_string());
        }
    }
    if let Some(v) = settings.get_mut("request_headers") {
        *v = json!(
            route
                .settings
                .request_headers
                .iter()
                .map(|(k, _)| (k, "[redacted]"))
                .collect::<Vec<_>>()
        );
    }
    if let Some(v) = settings.get_mut("response_headers") {
        *v = json!(
            route
                .settings
                .response_headers
                .iter()
                .map(|h| json!({"name":h.name,"value":"[redacted]","always":h.always}))
                .collect::<Vec<_>>()
        );
    }
    for pointer in [
        "/gateway/request_headers/set",
        "/gateway/request_headers/add",
        "/gateway/response_headers/set",
        "/gateway/response_headers/add",
    ] {
        if let Some(headers) = settings.pointer_mut(pointer).and_then(Value::as_array_mut) {
            for header in headers {
                header["value"] = json!("[redacted]");
            }
        }
    }
    json!({"id":route.id,"match":route.matcher,"action":route.action,"settings":settings,"script":route.script.as_ref().map(|s|&s.digest),"allowed_backends":route.allowed_backends,"tenant":route.tenant.as_ref().map(|t|t.diagnostic()),"traffic":route.rollout.as_ref().map(|r|r.diagnostic())})
}
fn routes(snapshot: &RuntimeSnapshot) -> Value {
    json!(snapshot.hosts.iter().map(|h|json!({"listener":h.listener,"names":h.names,"default":h.default,"source":if snapshot.gateway.is_some() {"gateway"} else if h.ingress {"ingress"} else {"file"},"routes":h.routes.iter().map(|r|route(r)).collect::<Vec<_>>()})).collect::<Vec<_>>())
}
fn backends(snapshot: &RuntimeSnapshot) -> Value {
    json!(
        snapshot
            .backends
            .iter()
            .map(|(name, b)| (name, b.diagnostic()))
            .collect::<BTreeMap<_, _>>()
    )
}
pub fn explain(
    snapshot: &RuntimeSnapshot,
    listener: SocketAddr,
    host: &str,
    path: &str,
) -> Result<Value> {
    let path = crate::proxy::normalized_path(path.split('?').next().unwrap_or("/"))?;
    let selected = snapshot.route(listener, host, &path);
    Ok(
        json!({"version":snapshot.version,"listener":listener,"host":host,"normalized_path":path,"selected":selected.as_ref().map(|r|route(r)),"result":if selected.is_some() {"matched"} else {"no route"}}),
    )
}

pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(
    session: &mut ServerSession,
) -> std::result::Result<T, (u16, Value)> {
    if session.req_header().method != http::Method::POST {
        return Err((405, json!({"error":"POST required"})));
    }
    session.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            match session.read_request_body().await {
                Ok(Some(chunk)) if bytes.len() + chunk.len() <= 1024 * 1024 => {
                    bytes.extend_from_slice(&chunk)
                }
                Ok(None) => return Ok(bytes),
                _ => {
                    return Err((
                        413,
                        json!({"error":"request body unavailable or exceeds 1 MiB"}),
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| (408, json!({"error":"request body deadline exceeded"})))??;
    serde_json::from_slice(&bytes).map_err(|_| (400, json!({"error":"invalid request JSON"})))
}
