mod body;
mod mirror;
pub(crate) mod planning;
mod responses;
mod static_files;
use crate::{
    model::*,
    runtime::Shared,
    script::{self, RequestData},
};
use async_trait::async_trait;
use bytes::Bytes;
use pingora::{
    http::{RequestHeader, ResponseHeader},
    prelude::*,
};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::Arc,
    time::{Instant, SystemTime},
};

pub struct Proxy {
    pub shared: Arc<Shared>,
    pub listener: SocketAddr,
    pub tls: bool,
}
pub struct Context {
    snapshot: Option<Arc<RuntimeSnapshot>>,
    route: Option<Arc<Route>>,
    request: RequestData,
    server_name: String,
    original_uri: String,
    backend: Option<String>,
    uri: Option<String>,
    edits: script::Edits,
    execution: Option<script::Execution>,
    body_bytes: u64,
    started: Instant,
    status: u16,
    request_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    plugin_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    upstream_address: Option<SocketAddr>,
    upstream_lease: Option<crate::backend::Lease>,
    original_peer: String,
    scheme: String,
    claims: BTreeMap<String, String>,
    traffic_permit: Option<crate::traffic::Permit>,
    pre_auth_permit: Option<crate::traffic::Permit>,
    grpc_status: Option<u16>,
    tenant_request: Option<crate::tenancy::Permit>,
    tenant_plugin: Option<crate::tenancy::Permit>,
    trace: Option<crate::otlp::trace::Trace>,
    affinity_cookie: Option<String>,
    rollout_stage: usize,
    mirror: Option<mirror::MirrorRequest>,
}
fn error(status: u16, message: impl Into<String>) -> Box<pingora::Error> {
    pingora::Error::explain(pingora::ErrorType::HTTPStatus(status), message.into())
}

pub fn normalized_path(path: &str) -> anyhow::Result<String> {
    anyhow::ensure!(path.len() <= 8192, "URI exceeds 8 KiB");
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            anyhow::ensure!(
                i + 2 < bytes.len()
                    && bytes[i + 1].is_ascii_hexdigit()
                    && bytes[i + 2].is_ascii_hexdigit(),
                "invalid percent escape"
            );
            i += 2;
        }
        i += 1;
    }
    let decoded = percent_encoding::percent_decode_str(path).decode_utf8()?;
    normalize_decoded_path(&decoded)
}

fn normalize_decoded_path(decoded: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        decoded.starts_with('/') && !decoded.contains(['\0', '\\', '\r', '\n']),
        "invalid path"
    );
    let mut components = vec![];
    for c in decoded.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                anyhow::ensure!(components.pop().is_some(), "path escapes root");
            }
            _ => components.push(c),
        }
    }
    let mut normalized = format!("/{}", components.join("/"));
    if (decoded.ends_with('/') || decoded.ends_with("/.") || decoded.ends_with("/.."))
        && normalized != "/"
    {
        normalized.push('/');
    }
    Ok(normalized)
}

#[async_trait]
impl ProxyHttp for Proxy {
    type CTX = Context;
    fn new_ctx(&self) -> Context {
        Context {
            snapshot: None,
            route: None,
            request: RequestData::default(),
            server_name: String::new(),
            original_uri: String::new(),
            backend: None,
            uri: None,
            edits: script::Edits::default(),
            execution: None,
            body_bytes: 0,
            started: Instant::now(),
            status: 0,
            request_permit: None,
            plugin_permit: None,
            upstream_address: None,
            upstream_lease: None,
            original_peer: String::new(),
            scheme: if self.tls { "https" } else { "http" }.into(),
            claims: BTreeMap::new(),
            traffic_permit: None,
            pre_auth_permit: None,
            grpc_status: None,
            tenant_request: None,
            tenant_plugin: None,
            trace: None,
            affinity_cookie: None,
            rollout_stage: 0,
            mirror: None,
        }
    }
    async fn request_filter(&self, session: &mut Session, ctx: &mut Context) -> Result<bool> {
        ctx.trace = crate::otlp::trace::Trace::new(
            &session.req_header().headers,
            self.shared.telemetry.trace_ratio,
        );
        ctx.snapshot = Some(self.shared.snapshot.load_full());
        ctx.original_uri = session
            .req_header()
            .uri
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_string();
        ctx.request.method = session.req_header().method.to_string();
        ctx.request.remote_addr = session
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.ip().to_string())
            .unwrap_or_default();
        ctx.request_permit = Some(self.shared.requests.clone().try_acquire_owned().map_err(
            |_| {
                self.shared
                    .telemetry
                    .rejected
                    .with_label_values(&["inflight"])
                    .inc();
                error(503, "in-flight request budget exhausted")
            },
        )?);
        let snapshot = ctx.snapshot.as_ref().unwrap().clone();
        let request = session.req_header();
        if request
            .headers
            .get_all("connection")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|s| s.split(','))
            .any(|v| {
                ["content-length", "transfer-encoding", "host"]
                    .contains(&v.trim().to_ascii_lowercase().as_str())
            })
        {
            return Err(error(
                400,
                "connection header names a framing or routing field",
            ));
        }
        let path = normalized_path(request.uri.path()).map_err(|e| error(400, e.to_string()))?;
        let host = request_host(request)?;
        if host.is_empty() {
            ctx.server_name = snapshot
                .hostless_server_name(self.listener)
                .unwrap_or_default()
                .to_owned();
        }
        ctx.original_uri = request
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string();
        ctx.request = RequestData {
            claims: BTreeMap::new(),
            method: request.method.to_string(),
            path: path.clone(),
            query: request.uri.query().unwrap_or("").to_string(),
            host: host.clone(),
            remote_addr: session
                .client_addr()
                .and_then(|a| a.as_inet())
                .map(|a| a.ip().to_string())
                .unwrap_or_default(),
            headers: header_map(&request.headers),
            body: None,
        };
        let Some(mut route) = snapshot.route(self.listener, &host, &path) else {
            return self
                .reply(session, ctx, 404, "not found\n".into(), None)
                .await;
        };
        let mut redirect = false;
        if !path.ends_with('/') && !matches!(route.matcher, PathMatch::Exact(_)) {
            let slash_path = format!("{path}/");
            if let Some(with_slash) = snapshot.route(self.listener, &host, &slash_path)
                && matches!(&with_slash.matcher, PathMatch::NginxPrefix(p) if p == &slash_path)
                && matches!(with_slash.action, Action::Proxy { .. })
            {
                route = with_slash;
                redirect = true;
            }
        }
        ctx.route = Some(route.clone());
        ctx.original_peer = ctx.request.remote_addr.clone();
        if let Ok(peer) = ctx.original_peer.parse() {
            let policy = &route.settings.identity;
            let proxy = session
                .digest()
                .and_then(|d| d.socket_digest.as_ref())
                .and_then(|d| d.proxy_protocol_addr.get())
                .and_then(|a| *a)
                .map(|a| a.ip());
            let client = policy.resolve(peer, &session.req_header().headers, proxy);
            ctx.request.remote_addr = client.to_string();
            ctx.scheme = policy
                .scheme(peer, &session.req_header().headers, self.tls)
                .into();
            if !policy.allows(client) {
                return Err(error(403, "client address denied"));
            }
        }
        if ctx.scheme != "https"
            && let Some(port) = route.settings.https_redirect_port
        {
            let location = https_redirect(&ctx.request.host, &ctx.original_uri, port);
            return self
                .reply(session, ctx, 308, String::new(), Some(location))
                .await;
        }
        let client_certificate = session
            .digest()
            .and_then(|d| d.ssl_digest.as_ref())
            .and_then(|d| d.extension.get::<crate::security::mtls::Peer>());
        if !route.settings.security.mtls.authorize(client_certificate) {
            return Err(error(
                403,
                "client certificate required or no longer trusted",
            ));
        }
        if let Some(tenant) = &route.tenant {
            ctx.tenant_request = Some(tenant.acquire(crate::tenancy::Resource::Request).map_err(
                |s| {
                    self.shared
                        .telemetry
                        .tenant_rejected
                        .with_label_values(&[tenant.name.as_str(), "request"])
                        .inc();
                    error(s, "namespace request quota exhausted")
                },
            )?);
            tenant.rate(&ctx.request).map_err(|s| {
                self.shared
                    .telemetry
                    .tenant_rejected
                    .with_label_values(&[tenant.name.as_str(), "rate"])
                    .inc();
                error(s, "namespace rate quota exhausted")
            })?;
        }
        let traffic = route
            .tenant
            .as_ref()
            .map_or(&self.shared.traffic, |t| &t.traffic);
        ctx.pre_auth_permit = traffic
            .acquire(
                &route.id,
                &route.settings.traffic.phase(true),
                &ctx.request,
                &ctx.claims,
            )
            .map_err(|status| {
                self.shared
                    .telemetry
                    .rejected
                    .with_label_values(&[if status == 429 {
                        "rate"
                    } else {
                        "route_concurrency"
                    }])
                    .inc();
                error(status, "pre-authentication traffic budget exhausted")
            })?;
        if let Some(jwt) = &route.settings.security.jwt {
            let mut tokens = session.req_header().headers.get_all("authorization").iter();
            let token = tokens
                .next()
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .filter(|_| tokens.next().is_none())
                .ok_or_else(|| error(401, "bearer token required"))?;
            ctx.claims = jwt
                .verify(token)
                .map_err(|_| error(401, "invalid bearer token"))?;
            ctx.request.claims = ctx.claims.clone();
        }
        if let Some(auth) = &route.settings.security.external {
            let _auth_budget = route
                .tenant
                .as_ref()
                .map(|t| t.acquire(crate::tenancy::Resource::Auth))
                .transpose()
                .map_err(|s| error(s, "namespace authentication quota exhausted"))?;
            for name in &auth.response_headers {
                ctx.request.headers.remove(name);
                ctx.edits.headers.insert(name.clone(), None);
            }
            let headers = auth
                .authorize(&self.shared.auth_client, &ctx.request, &ctx.original_uri)
                .await
                .map_err(|status| error(status, "external authorization rejected"))?;
            for (name, value) in headers {
                ctx.request.headers.insert(name.clone(), value.clone());
                ctx.edits.headers.insert(name, Some(value));
            }
        }
        ctx.traffic_permit = traffic
            .acquire(
                &route.id,
                &route.settings.traffic.phase(false),
                &ctx.request,
                &ctx.claims,
            )
            .map_err(|status| {
                self.shared
                    .telemetry
                    .rejected
                    .with_label_values(&[if status == 429 {
                        "rate"
                    } else {
                        "route_concurrency"
                    }])
                    .inc();
                error(status, "route traffic budget exhausted")
            })?;
        let keepalive = route.settings.keepalive;
        let keepalive_seconds = keepalive
            .as_secs()
            .saturating_add(u64::from(keepalive.subsec_nanos() > 0));
        // Preserve parser decisions such as disabling reuse for ambiguous body framing.
        if session.get_keepalive().is_some() {
            session.set_keepalive((!keepalive.is_zero()).then_some(keepalive_seconds));
        }
        session.set_read_timeout(Some(route.settings.read_timeout));
        session.set_write_timeout(Some(route.settings.write_timeout));
        if redirect {
            let query = if ctx.request.query.is_empty() {
                String::new()
            } else {
                format!("?{}", ctx.request.query)
            };
            return self
                .reply(
                    session,
                    ctx,
                    301,
                    String::new(),
                    Some(format!("{}{query}", encode_path(&format!("{path}/")))),
                )
                .await;
        }
        if route.settings.max_body > 0
            && session
                .req_header()
                .headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|n| n > route.settings.max_body)
        {
            return Err(error(413, "request body too large"));
        }
        ctx.rollout_stage = route.rollout.as_ref().map_or(0, |r| r.stage());
        let action = route.action.clone();
        if let Some(plugin) = &route.script {
            ctx.tenant_plugin = route
                .tenant
                .as_ref()
                .map(|t| t.acquire(crate::tenancy::Resource::Plugin))
                .transpose()
                .map_err(|s| error(s, "namespace plugin quota exhausted"))?;
            ctx.plugin_permit = Some(self.shared.plugins.clone().try_acquire_owned().map_err(
                |_| {
                    self.shared
                        .telemetry
                        .rejected
                        .with_label_values(&["plugin"])
                        .inc();
                    error(503, "plugin instance budget exhausted")
                },
            )?);
            let mut plugin_request = ctx.request.clone();
            plugin_request.body =
                body::inspect(session, route.settings.body_policy, route.settings.max_body).await?;
            self.shared.telemetry.plugin_calls.inc();
            let result = {
                let _timer = self.shared.telemetry.plugin_duration.start_timer();
                plugin.request(plugin_request)
            };
            match result {
                Ok((execution, outcome)) => {
                    ctx.execution = Some(execution);
                    ctx.edits.path = outcome.edits.path;
                    ctx.edits.query = outcome.edits.query;
                    ctx.edits.headers.extend(outcome.edits.headers);
                    match outcome.decision {
                        script::Decision::Pass => {}
                        script::Decision::Proxy(name) => {
                            let Some(key) = route.allowed_backends.get(&name) else {
                                self.shared.telemetry.plugin_errors.inc();
                                return Err(error(500, "plugin selected an undeclared backend"));
                            };
                            ctx.backend = Some(key.clone());
                        }
                        script::Decision::Reply(status, body) => {
                            return self.reply(session, ctx, status, body, None).await;
                        }
                    }
                }
                Err(e) => {
                    self.shared.telemetry.plugin_errors.inc();
                    log::error!("plugin {}: {e:#}", route.id);
                    return Err(error(500, "plugin execution failed"));
                }
            }
        }
        if let Some(backend) = ctx.backend.take() {
            ctx.backend = Some(
                route
                    .rollout
                    .as_ref()
                    .map_or_else(|| backend.clone(), |r| r.enforce(backend.clone())),
            );
            return Ok(false);
        }
        match action {
            Action::Proxy { backend, uri } => {
                ctx.backend = Some(
                    route
                        .rollout
                        .as_ref()
                        .map_or(backend, |r| r.select(&ctx.request)),
                );
                ctx.uri = uri;
                Ok(false)
            }
            Action::Return { status, text } => {
                let text = expand(&text, ctx, self.tls, "");
                let redirect = (300..400).contains(&status) && !text.is_empty();
                let location = if redirect { Some(text.clone()) } else { None };
                self.reply(
                    session,
                    ctx,
                    status,
                    if redirect { String::new() } else { text },
                    location,
                )
                .await
            }
            Action::Unavailable => Err(error(503, "backend unavailable")),
            Action::Static => self.serve_file(session, ctx, &route).await,
        }
    }
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Context,
    ) -> Result<Box<HttpPeer>> {
        if let Some(trace) = &mut ctx.trace {
            trace.upstream_start = Some(crate::otlp::trace::now());
        }
        let backend = ctx
            .snapshot
            .as_ref()
            .and_then(|s| ctx.backend.as_ref().and_then(|key| s.backends.get(key)))
            .ok_or_else(|| error(503, "backend unavailable"))?;
        let key = match &backend.options.balance {
            crate::backend::Balance::Hash(key) => key.value(&ctx.request, &ctx.claims),
            crate::backend::Balance::Sticky(name) => {
                let existing = ctx.request.headers.get("cookie").and_then(|v| {
                    v.split(';')
                        .filter_map(|v| v.trim().split_once('='))
                        .find(|(k, v)| {
                            k == name && v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                        .map(|(_, v)| v.to_owned())
                });
                existing.unwrap_or_else(|| {
                    let value = format!("{:032x}", rand::random::<u128>());
                    ctx.affinity_cookie = Some(format!(
                        "{name}={value}; Path=/; Max-Age=86400; HttpOnly; SameSite=Lax{}",
                        if ctx.scheme == "https" {
                            "; Secure"
                        } else {
                            ""
                        }
                    ));
                    value
                })
            }
            _ => String::new(),
        };
        let lease = backend
            .select(&key)
            .ok_or_else(|| error(503, "no ready endpoint or backend budget exhausted"))?;
        let address = lease.address;
        ctx.upstream_lease = Some(lease);
        ctx.upstream_address = Some(address);
        let settings = &ctx.route.as_ref().unwrap().settings;
        let mut peer = HttpPeer::new(
            address,
            backend.tls,
            settings
                .upstream
                .server_name
                .clone()
                .unwrap_or_else(|| backend.hostname.clone()),
        );
        let (max, min) = settings.upstream.protocol.versions();
        peer.options.set_http_version(max, min);
        peer.options.ca = settings.upstream.ca.clone();
        peer.client_cert_key = settings.upstream.identity.clone();
        // Pingora's reuse key omits the custom CA and ALPN policy. Keep pools
        // separate so a CA withdrawal cannot reuse a previously trusted socket.
        peer.group_key = settings.upstream.pool_key();
        peer.options.connection_timeout = Some(settings.connect_timeout);
        peer.options.read_timeout = Some(settings.read_timeout);
        peer.options.write_timeout = Some(settings.write_timeout);
        peer.options.idle_timeout = Some(settings.keepalive);
        peer.options.verify_cert = true;
        peer.options.verify_hostname = true;
        Ok(Box::new(peer))
    }
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        request: &mut RequestHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        let snapshot = ctx.snapshot.as_ref().unwrap();
        let backend = &snapshot.backends[ctx.backend.as_ref().unwrap()];
        let route = ctx.route.as_ref().unwrap();
        let target = planning::outbound_uri(
            &ctx.original_uri,
            &ctx.request,
            &ctx.edits,
            &route.matcher,
            ctx.uri.as_deref(),
        );
        let target: http::Uri = target
            .parse()
            .map_err(|_| error(500, "invalid rewritten URI"))?;
        request.set_uri(target);
        strip_hop_headers(request);
        if ctx.request.headers.get("te").is_some_and(|v| {
            v.split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("trailers"))
        }) {
            request.insert_header("TE", "trailers")?;
        }
        request.insert_header("Host", backend.host_header.as_str())?;
        for (name, value) in &route.settings.request_headers {
            let value = expand(value, ctx, self.tls, &backend.host_header);
            if value.is_empty() {
                request.remove_header(name);
            } else {
                request.insert_header(name.clone(), value)?;
            }
        }
        for (name, value) in &ctx.edits.headers {
            if let Some(value) = value {
                request.insert_header(name.clone(), value.as_str())?;
            } else {
                request.remove_header(name);
            }
        }
        if let Some(trace) = &ctx.trace
            && trace.enabled
        {
            request.insert_header("traceparent", trace.header())?;
            if request
                .headers
                .get("tracestate")
                .is_some_and(|v| v.len() > 512)
            {
                request.remove_header("tracestate");
            }
        }
        // Upgrade headers are transport state and cannot be manufactured by a script.
        if ctx
            .request
            .headers
            .get("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            request.insert_header("Upgrade", "websocket")?;
            request.insert_header("Connection", "upgrade")?;
        }
        self.prepare_mirror(session, request, ctx);
        Ok(())
    }
    async fn request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        end: bool,
        ctx: &mut Context,
    ) -> Result<()> {
        // Pingora also delivers upgraded protocol frames through this hook.
        if session.was_upgraded() {
            return Ok(());
        }
        ctx.body_bytes = ctx
            .body_bytes
            .saturating_add(body.as_ref().map_or(0, |b| b.len() as u64));
        if ctx
            .route
            .as_ref()
            .is_some_and(|r| r.settings.max_body > 0 && ctx.body_bytes > r.settings.max_body)
        {
            return Err(error(413, "request body too large"));
        }
        self.mirror_body(body, end, ctx);
        Ok(())
    }
    async fn response_filter(
        &self,
        session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        if let Some(value) = response.headers.get("grpc-status") {
            ctx.grpc_status = Some(
                value
                    .to_str()
                    .ok()
                    .and_then(|v| v.parse::<u16>().ok())
                    .filter(|v| *v <= 16)
                    .unwrap_or(2),
            );
        }
        self.response_headers(response, ctx)?;
        if let Some(route) = &ctx.route {
            crate::compression::prepare(session, response, &route.settings.compression);
        }
        Ok(())
    }
    async fn response_trailer_filter(
        &self,
        _session: &mut Session,
        trailers: &mut http::HeaderMap,
        ctx: &mut Context,
    ) -> Result<Option<Bytes>> {
        if let Some(value) = trailers.get("grpc-status") {
            ctx.grpc_status = Some(
                value
                    .to_str()
                    .ok()
                    .and_then(|v| v.parse::<u16>().ok())
                    .filter(|v| *v <= 16)
                    .unwrap_or(2),
            );
        }
        Ok(None)
    }
    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &pingora::Error,
        ctx: &mut Context,
    ) -> pingora::proxy::FailToProxy {
        use pingora::{ErrorSource, ErrorType};
        let mut code = match e.etype() {
            ErrorType::HTTPStatus(code) => *code,
            _ => match e.esource() {
                ErrorSource::Upstream => match e.etype() {
                    ErrorType::ConnectTimedout
                    | ErrorType::TLSHandshakeTimedout
                    | ErrorType::ReadTimedout
                    | ErrorType::WriteTimedout => 504,
                    _ => 502,
                },
                ErrorSource::Downstream => match e.etype() {
                    ErrorType::WriteError | ErrorType::ReadError | ErrorType::ConnectionClosed => 0,
                    _ => 400,
                },
                _ => 500,
            },
        };
        session.set_keepalive(None);
        if code > 0 && session.response_written().is_none() {
            let mut response = ResponseHeader::build(code, None).unwrap();
            response.insert_header("Content-Length", "0").unwrap();
            if code == 429 || code == 503 {
                response.insert_header("Retry-After", "1").unwrap();
            }
            if code == 401 {
                response
                    .insert_header("WWW-Authenticate", "Bearer")
                    .unwrap();
            }
            if self.response_headers(&mut response, ctx).is_err() {
                code = 500;
                // Failed hooks must not be retried or leak their staged changes.
                response = ResponseHeader::build(code, None).unwrap();
                response.insert_header("Content-Length", "0").unwrap();
                let _ = self.response_headers(&mut response, ctx);
            }
            ctx.status = code;
            if let Err(error) = session
                .write_response_header(Box::new(response), true)
                .await
            {
                log::debug!("error response write: {error}");
            }
        }
        pingora::proxy::FailToProxy {
            error_code: code,
            can_reuse_downstream: false,
        }
    }
    async fn logging(&self, session: &mut Session, e: Option<&pingora::Error>, ctx: &mut Context) {
        if let Some(address) = ctx.upstream_address
            && let Some(backend) = ctx
                .snapshot
                .as_ref()
                .and_then(|s| ctx.backend.as_ref().and_then(|name| s.backends.get(name)))
            && (e.is_none()
                || e.is_some_and(|error| error.esource() == &pingora::ErrorSource::Upstream))
            && backend.record_result(
                address,
                e.is_some(),
                self.shared.upstream_max_fails,
                self.shared.upstream_fail_timeout,
            )
        {
            self.shared.telemetry.upstream_ejections.inc();
            log::warn!(
                "temporarily excluding upstream {address} after repeated transport failures"
            );
        }
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(if ctx.status != 0 { ctx.status } else { 502 });
        self.shared
            .telemetry
            .requests
            .with_label_values(&[&status.to_string()])
            .inc();
        self.shared
            .telemetry
            .duration
            .observe(ctx.started.elapsed().as_secs_f64());
        let route_id = ctx.route.as_ref().map_or("_unmatched", |r| r.id.as_str());
        let upstream_failed = e.is_some_and(|e| e.esource() == &pingora::ErrorSource::Upstream);
        let grpc = ctx
            .request
            .headers
            .get("content-type")
            .is_some_and(|v| v.starts_with("application/grpc"));
        let grpc_status = grpc.then(|| {
            ctx.grpc_status.unwrap_or(match status {
                401 => 16,
                403 => 7,
                429 | 502..=504 => 14,
                _ => 2,
            })
        });
        if let Some(rollout) = ctx.route.as_ref().and_then(|r| r.rollout.as_ref())
            && let Some(backend) = &ctx.backend
            && (e.is_none() || upstream_failed)
            && rollout.completed(
                backend,
                upstream_failed || status >= 500 || grpc_status.is_some_and(|s| s != 0),
                ctx.started.elapsed(),
                ctx.rollout_stage,
            )
        {
            self.shared.telemetry.rollbacks.inc();
            log::warn!(
                "traffic revision {} rolled back for {}",
                rollout.policy.revision,
                rollout.owner
            );
        }
        self.shared.telemetry.completed(
            route_id,
            ctx.backend.as_deref(),
            status,
            ctx.started.elapsed().as_secs_f64(),
            upstream_failed || status >= 500 || grpc_status.is_some_and(|s| s != 0),
            grpc_status,
        );
        if let (Some(trace), Some(exporter)) = (&ctx.trace, &self.shared.telemetry.traces) {
            trace.export(
                exporter,
                route_id,
                &ctx.request.method,
                (status, grpc_status),
                ctx.backend.as_deref(),
                upstream_failed,
            );
        }
        if let Some(e) = e {
            if e.esource() == &pingora::ErrorSource::Upstream {
                self.shared.telemetry.upstream_errors.inc();
            }
            log::warn!(
                "request failed route={} backend={} upstream={:?} version={}: {e}",
                ctx.route.as_ref().map_or("-", |r| r.id.as_str()),
                ctx.backend.as_deref().unwrap_or("-"),
                ctx.upstream_address,
                ctx.snapshot.as_ref().map_or(0, |s| s.version)
            );
        }
        if let Some(path) = ctx
            .route
            .as_ref()
            .map(|r| r.settings.access_log.clone())
            .unwrap_or_else(|| {
                ctx.snapshot
                    .as_ref()
                    .and_then(|s| s.default_access_log.clone())
            })
        {
            if let Some(exporter) = &self.shared.telemetry.otlp {
                exporter.access(crate::otlp::AccessRecord {
                    trace: ctx.trace.as_ref(),
                    method: &ctx.request.method,
                    path: ctx.original_uri.split('?').next().unwrap_or("/"),
                    host: &ctx.request.host,
                    client: if ctx
                        .route
                        .as_ref()
                        .is_none_or(|r| r.settings.log_policy.client)
                    {
                        &ctx.request.remote_addr
                    } else {
                        ""
                    },
                    protocol_version: match session.req_header().version {
                        http::Version::HTTP_09 => "0.9",
                        http::Version::HTTP_10 => "1.0",
                        http::Version::HTTP_11 => "1.1",
                        http::Version::HTTP_2 => "2",
                        _ => "unknown",
                    },
                    tls: self.tls,
                    status,
                    response_bytes: session.body_bytes_sent(),
                    duration: ctx.started.elapsed(),
                    route: ctx.route.as_ref().map_or("-", |r| r.id.as_str()),
                    backend: ctx.backend.as_deref(),
                    upstream: ctx.upstream_address,
                    config_hash: ctx
                        .snapshot
                        .as_ref()
                        .map_or("-", |s| s.content_hash.as_str()),
                    config_version: ctx.snapshot.as_ref().map_or(0, |s| s.version),
                    error_source: e.map(|e| match e.esource() {
                        pingora::ErrorSource::Upstream => "upstream",
                        pingora::ErrorSource::Downstream => "downstream",
                        _ => "internal",
                    }),
                });
            }
            let policy = ctx
                .route
                .as_ref()
                .map(|r| r.settings.log_policy.clone())
                .unwrap_or_default();
            let line = policy.render(serde_json::json!({
                "timestamp": chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).format("%d/%b/%Y:%H:%M:%S +0000").to_string(),
                "client":ctx.request.remote_addr,"method":ctx.request.method,"uri":ctx.original_uri,
                "protocol":format!("{:?}",session.req_header().version),"status":status,"bytes":session.body_bytes_sent(),
                "referer":ctx.request.headers.get("referer").map_or("-",String::as_str),
                "user_agent":ctx.request.headers.get("user-agent").map_or("-",String::as_str),
                "route":route_id,"backend":ctx.backend.as_deref().unwrap_or("-"),
                "upstream":ctx.upstream_address.map_or_else(||"-".into(),|a|a.to_string()),
                "config":ctx.snapshot.as_ref().map_or("-",|s|s.content_hash.as_str()),
                "trace_id":ctx.trace.as_ref().map_or_else(||"-".into(),|t|crate::otlp::trace::hex(&t.trace_id)),
                "span_id":ctx.trace.as_ref().map_or_else(||"-".into(),|t|crate::otlp::trace::hex(&t.span_id)),
                "grpc_status":grpc_status
            }));
            self.shared.telemetry.access(path, line);
        }
    }
}

fn request_host(request: &RequestHeader) -> Result<String> {
    let parse = |value: &str| -> Result<String> {
        let authority = value
            .parse::<http::uri::Authority>()
            .map_err(|_| error(400, "invalid request authority"))?;
        if authority.host().is_empty() || value.contains('@') {
            return Err(error(400, "invalid request authority"));
        }
        Ok(authority.host().to_ascii_lowercase())
    };
    let mut fields = request.headers.get_all("host").iter();
    let host = fields
        .next()
        .map(|value| {
            let value = value
                .to_str()
                .map_err(|_| error(400, "invalid Host header"))?;
            parse(value)
        })
        .transpose()?;
    if fields.next().is_some() || (request.version == http::Version::HTTP_11 && host.is_none()) {
        return Err(error(400, "HTTP/1.1 requires exactly one Host header"));
    }
    // HTTP/2 may supply :authority without an HTTP/1 Host header.
    Ok(request
        .uri
        .authority()
        .map(|authority| parse(authority.as_str()))
        .transpose()?
        .or(host)
        .unwrap_or_default())
}

fn header_map(headers: &http::HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect()
}
fn encode_path(path: &str) -> String {
    const ESCAPE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'%')
        .add(b'?')
        .add(b'<')
        .add(b'>')
        .add(b'`')
        .add(b'{')
        .add(b'}');
    percent_encoding::utf8_percent_encode(path, ESCAPE).to_string()
}
fn strip_hop_headers(request: &mut RequestHeader) {
    let connection: Vec<String> = request
        .headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(',').map(|s| s.trim().to_ascii_lowercase()))
        .collect();
    for key in connection {
        request.remove_header(&key);
    }
    for key in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "upgrade",
        "te",
        "trailer",
    ] {
        request.remove_header(key);
    }
}
fn expand(input: &str, ctx: &Context, _tls: bool, proxy_host: &str) -> String {
    planning::Variables {
        request: &ctx.request,
        edits: &ctx.edits,
        original_uri: &ctx.original_uri,
        original_peer: &ctx.original_peer,
        scheme: &ctx.scheme,
        server_name: &ctx.server_name,
    }
    .expand(input, proxy_host)
}

pub(crate) fn https_redirect(host: &str, uri: &str, port: u16) -> String {
    let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let port = if port == 443 {
        String::new()
    } else {
        format!(":{port}")
    };
    let path = uri
        .parse::<http::Uri>()
        .ok()
        .and_then(|u| u.path_and_query().map(|p| p.as_str().to_owned()))
        .unwrap_or_else(|| "/".into());
    format!("https://{host}{port}{path}")
}
