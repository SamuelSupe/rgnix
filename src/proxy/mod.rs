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
        }
    }
    async fn request_filter(&self, session: &mut Session, ctx: &mut Context) -> Result<bool> {
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
        let snapshot = self.shared.snapshot.load_full();
        ctx.snapshot = Some(snapshot.clone());
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
        let action = route.action.clone();
        if let Some(plugin) = &route.script {
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
            self.shared.telemetry.plugin_calls.inc();
            let result = {
                let _timer = self.shared.telemetry.plugin_duration.start_timer();
                plugin.request(ctx.request.clone())
            };
            match result {
                Ok((execution, outcome)) => {
                    ctx.execution = Some(execution);
                    ctx.edits = outcome.edits;
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
        if ctx.backend.is_some() {
            return Ok(false);
        }
        match action {
            Action::Proxy { backend, uri } => {
                ctx.backend = Some(backend);
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
        let backend = ctx
            .snapshot
            .as_ref()
            .and_then(|s| ctx.backend.as_ref().and_then(|key| s.backends.get(key)))
            .ok_or_else(|| error(503, "backend unavailable"))?;
        let address = backend
            .select()
            .ok_or_else(|| error(503, "no ready endpoints"))?;
        ctx.upstream_address = Some(address);
        let settings = &ctx.route.as_ref().unwrap().settings;
        let mut peer = HttpPeer::new(address, backend.tls, backend.hostname.clone());
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
        _session: &mut Session,
        request: &mut RequestHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        let snapshot = ctx.snapshot.as_ref().unwrap();
        let backend = &snapshot.backends[ctx.backend.as_ref().unwrap()];
        let route = ctx.route.as_ref().unwrap();
        let path = if let Some(path) = &ctx.edits.path {
            encode_path(path)
        } else if let Some(uri) = &ctx.uri {
            let suffix = ctx
                .request
                .path
                .strip_prefix(route.matcher.path())
                .unwrap_or(&ctx.request.path);
            format!("{uri}{}", encode_path(suffix))
        } else if ctx.edits.query.is_some() {
            ctx.original_uri
                .split_once('?')
                .map_or(ctx.original_uri.as_str(), |(path, _)| path)
                .to_owned()
        } else {
            ctx.original_uri.clone()
        };
        let rewritten = ctx.edits.path.is_some() || ctx.uri.is_some() || ctx.edits.query.is_some();
        let target = if rewritten {
            let query = ctx.edits.query.as_deref().unwrap_or(&ctx.request.query);
            if !query.is_empty() && !path.contains('?') {
                format!("{path}?{query}")
            } else {
                path
            }
        } else {
            path
        };
        let target: http::Uri = target
            .parse()
            .map_err(|_| error(500, "invalid rewritten URI"))?;
        request.set_uri(target);
        strip_hop_headers(request);
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
        Ok(())
    }
    async fn request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        _end: bool,
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
        Ok(())
    }
    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        self.response_headers(response, ctx)
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
            .and_then(|r| r.settings.access_log.clone())
        {
            let escape = |s: &str| {
                s.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace(['\r', '\n'], " ")
            };
            self.shared.telemetry.access(
                path,
                format!(
                    "{} - - [{}] \"{} {} {:?}\" {} {} \"{}\" \"{}\" route=\"{}\" backend=\"{}\" upstream=\"{}\" config=\"{}\"",
                    ctx.request.remote_addr,
                    chrono::DateTime::<chrono::Utc>::from(SystemTime::now())
                        .format("%d/%b/%Y:%H:%M:%S +0000"),
                    ctx.request.method,
                    escape(&ctx.original_uri),
                    session.req_header().version,
                    status,
                    session.body_bytes_sent(),
                    escape(
                        ctx.request
                            .headers
                            .get("referer")
                            .map(String::as_str)
                            .unwrap_or("-")
                    ),
                    escape(
                        ctx.request
                            .headers
                            .get("user-agent")
                            .map(String::as_str)
                            .unwrap_or("-")
                    ),
                    escape(&ctx.route.as_ref().map_or_else(|| "-".into(), |r| r.id.clone())),
                    escape(ctx.backend.as_deref().unwrap_or("-")),
                    ctx.upstream_address.map_or_else(|| "-".into(), |a| a.to_string()),
                    ctx.snapshot.as_ref().map_or("-", |s| s.content_hash.as_str()),
                ),
            );
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
fn expand(input: &str, ctx: &Context, tls: bool, proxy_host: &str) -> String {
    let mut output = String::new();
    let mut rest = input;
    while let Some(i) = rest.find('$') {
        output.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let key = &rest[..end];
        let value = match key {
            "host" if ctx.request.host.is_empty() => ctx.server_name.clone(),
            "host" => ctx.request.host.clone(),
            "http_host" => ctx.request.headers.get("host").cloned().unwrap_or_default(),
            "scheme" => if tls { "https" } else { "http" }.into(),
            "request_uri" => ctx.original_uri.clone(),
            "uri" => ctx.edits.path.as_ref().unwrap_or(&ctx.request.path).clone(),
            "args" => ctx
                .edits
                .query
                .as_ref()
                .unwrap_or(&ctx.request.query)
                .clone(),
            "request_method" => ctx.request.method.clone(),
            "remote_addr" => ctx.request.remote_addr.clone(),
            "proxy_host" => proxy_host.into(),
            "proxy_add_x_forwarded_for" => ctx
                .request
                .headers
                .get("x-forwarded-for")
                .map(|v| format!("{v}, {}", ctx.request.remote_addr))
                .unwrap_or_else(|| ctx.request.remote_addr.clone()),
            _ => key
                .strip_prefix("http_")
                .and_then(|s| {
                    ctx.request
                        .headers
                        .get(&s.replace('_', "-").to_ascii_lowercase())
                })
                .cloned()
                .unwrap_or_default(),
        };
        output.push_str(&value);
        rest = &rest[end..];
    }
    output.push_str(rest);
    output
}
