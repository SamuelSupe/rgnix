use super::{
    Listener,
    body::{Counters, Payload, RequestBody, RequestGuard, ResponseBody},
    io::Connection,
};
use crate::{
    model::{Action, PathMatch},
    proxy::{Proxy, error, expand, normalized_path, planning, request_host_parts},
    script,
};
use bytes::Bytes;
use http_body_util::{Either, Full};
use hyper::{Request, Response, body::Incoming};
use pingora::proxy::ProxyHttp;
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

pub(super) async fn serve(
    listener: &Listener,
    request: Request<Incoming>,
    peer: SocketAddr,
    connection: Arc<Connection>,
) -> Result<Response<ResponseBody>, Infallible> {
    let proxy = Proxy {
        shared: listener.shared.clone(),
        listener: listener.address,
        tls: false,
    };
    let mut guard = Box::new(RequestGuard {
        ctx: proxy.new_ctx(),
        proxy,
        finished: false,
        counts: Arc::new(Counters::default()),
        version: request.version(),
        error: None,
        keepalive: Duration::from_secs(60),
        io_failure: connection.failure.clone(),
    });
    let ctx = &mut guard.ctx;
    ctx.snapshot = Some(listener.shared.snapshot.load_full());
    ctx.trace =
        crate::otlp::trace::Trace::new(request.headers(), listener.shared.telemetry.trace_ratio)
            .map(Box::new);
    ctx.original_uri = request
        .uri()
        .path_and_query()
        .map_or("/", |p| p.as_str())
        .into();
    ctx.request.method = request.method().to_string();
    ctx.request.remote_addr = peer.ip().to_string();
    ctx.original_peer = ctx.request.remote_addr.clone();
    connection.read_timeout_ms.store(60_000, Ordering::Relaxed);
    let result = forward(listener, request, peer, &mut guard).await;
    let mut response = match result {
        Ok(response) => response,
        Err(failure) => {
            let status = match failure.etype() {
                pingora::ErrorType::HTTPStatus(status) => *status,
                pingora::ErrorType::Custom("UpstreamTimedout")
                | pingora::ErrorType::ConnectTimedout
                | pingora::ErrorType::ReadTimedout
                | pingora::ErrorType::WriteTimedout => 504,
                _ => 502,
            };
            guard.error = Some(failure);
            reply(
                status,
                format!(
                    "{}\n",
                    http::StatusCode::from_u16(status)
                        .unwrap()
                        .canonical_reason()
                        .unwrap_or("request failed")
                ),
                None,
            )
        }
    };
    let ctx = &mut guard.ctx;
    ctx.status = response.status().as_u16();
    if let Some(route) = &ctx.route {
        guard.keepalive = route.settings.keepalive;
        let prepared = ctx
            .snapshot
            .as_ref()
            .unwrap()
            .hyper
            .as_ref()
            .unwrap()
            .route(listener.address, route);
        for (name, value, always) in &prepared.response_headers {
            if *always || [200, 201, 204, 206, 301, 302, 303, 304, 307, 308].contains(&ctx.status) {
                match value.expand(ctx, "") {
                    Ok(value) if !value.is_empty() => {
                        response.headers_mut().append(name.clone(), value);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        response = reply(500, "invalid expanded response header\n".into(), None);
                        ctx.status = 500;
                        break;
                    }
                }
            }
        }
    }

    if ctx.request.method == "HEAD" || [204, 304].contains(&ctx.status) {
        *response.body_mut() = Either::Right(Full::new(Bytes::new()));
    }
    if guard.error.is_some()
        || listener.shared.telemetry.draining.get() != 0
        || guard.keepalive.is_zero()
    {
        response
            .headers_mut()
            .insert("connection", http::HeaderValue::from_static("close"));
    }
    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(
        parts,
        ResponseBody::new(body, guard, connection),
    ))
}

async fn forward(
    listener: &Listener,
    mut request: Request<Incoming>,
    peer: SocketAddr,
    guard: &mut RequestGuard,
) -> pingora::Result<Response<Payload>> {
    let ctx = &mut guard.ctx;
    if listener.shared.telemetry.draining.get() != 0 {
        return Err(error(503, "server is draining"));
    }
    ctx.request_permit = Some(
        listener
            .shared
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                listener
                    .shared
                    .telemetry
                    .rejected
                    .with_label_values(&["inflight"])
                    .inc();
                error(503, "in-flight request budget exhausted")
            })?,
    );
    if request.method() == http::Method::CONNECT
        || request.headers().contains_key("upgrade")
        || request.headers().contains_key("trailer")
    {
        return Err(error(
            501,
            "CONNECT, upgrade and request trailers are not supported by experimental Hyper",
        ));
    }
    crate::proxy::validate_connection_header(request.headers())?;
    ctx.request.host = request_host_parts(request.headers(), request.uri(), request.version())?;
    ctx.request.path =
        normalized_path(request.uri().path()).map_err(|e| error(400, e.to_string()))?;
    ctx.request.query = request.uri().query().unwrap_or("").into();
    let snapshot = ctx.snapshot.as_ref().unwrap();
    ctx.server_name = snapshot
        .hostless_server_name(listener.address)
        .unwrap_or_default()
        .into();
    let Some(mut route) = snapshot.route_request(listener.address, &ctx.request) else {
        ctx.request.headers = script::RequestHeaders::from_selected(
            request.headers(),
            &[
                http::header::CONTENT_TYPE,
                http::header::REFERER,
                http::header::USER_AGENT,
            ],
        );
        return Ok(reply(404, "not found\n".into(), None));
    };
    let mut redirect = false;
    if !ctx.request.path.ends_with('/') && !matches!(route.matcher, PathMatch::Exact(_)) {
        let slash = format!("{}/", ctx.request.path);
        if let Some(other) = snapshot.route(listener.address, &ctx.request.host, &slash)
            && matches!(&other.matcher, PathMatch::NginxPrefix(p) if p == &slash)
            && matches!(other.action, Action::Proxy { .. })
        {
            route = other;
            redirect = true;
        }
    }
    ctx.route = Some(route.clone());
    let prepared = snapshot
        .hyper
        .as_ref()
        .unwrap()
        .route(listener.address, &route);
    ctx.request.headers = match &prepared.original_headers {
        Some(names) => script::RequestHeaders::from_selected(request.headers(), names),
        None => script::RequestHeaders::from_http(request.headers()),
    };
    let settings = &route.settings;
    let client = settings
        .identity
        .resolve(peer.ip(), request.headers(), None);
    if client != peer.ip() {
        ctx.request.remote_addr = client.to_string();
    }
    ctx.scheme = settings
        .identity
        .scheme(peer.ip(), request.headers(), false)
        .into();
    if !settings.identity.allows(client) {
        return Err(error(403, "client address denied"));
    }
    ctx.traffic_permit = listener
        .shared
        .traffic
        .acquire(&route.id, &settings.traffic, &ctx.request, &ctx.claims)
        .map_err(|status| error(status, "route rate or concurrency budget exhausted"))?;
    if ctx.scheme != "https"
        && let Some(port) = settings.https_redirect_port
    {
        return Ok(reply(
            308,
            String::new(),
            Some(crate::proxy::https_redirect(
                &ctx.request.host,
                &ctx.original_uri,
                port,
            )),
        ));
    }
    if redirect {
        let query = if ctx.request.query.is_empty() {
            String::new()
        } else {
            format!("?{}", ctx.request.query)
        };
        return Ok(reply(
            301,
            String::new(),
            Some(format!(
                "{}{query}",
                crate::proxy::encode_path(route.matcher.path())
            )),
        ));
    }
    if settings.max_body > 0
        && request
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|n| n > settings.max_body)
    {
        return Err(error(413, "request body too large"));
    }
    let (backend_name, uri) = match &route.action {
        Action::Return { status, text } => {
            let text = expand(text, ctx, false, "");
            let redirect = (300..400).contains(status) && !text.is_empty();
            return Ok(reply(
                *status,
                if redirect {
                    String::new()
                } else {
                    text.clone()
                },
                redirect.then_some(text),
            ));
        }
        Action::Proxy { backend, uri } => (backend, uri.as_deref()),
        _ => return Err(error(503, "backend unavailable")),
    };
    let target = prepared.backend.as_ref().unwrap();
    let backend = &target.backend;
    let key = match &backend.options.balance {
        crate::backend::Balance::Hash(key) => key.value(&ctx.request, &ctx.claims),
        _ => String::new(),
    };
    let lease = backend
        .select(&key)
        .ok_or_else(|| error(503, "no ready endpoint or backend budget exhausted"))?;
    let address = lease.address;
    ctx.backend = Some(backend_name.clone());
    ctx.upstream_lease = Some(lease);
    ctx.upstream_address = Some(address);
    ctx.upstream_started = Some(Instant::now());
    ctx.upstream_label = Some(
        listener
            .shared
            .telemetry
            .label("backend", backend_name)
            .into(),
    );
    if let Some(trace) = &mut ctx.trace {
        trace.upstream = Some(trace.client(&ctx.request.method, backend_name, "proxy"));
    }
    let path = if uri.is_none() {
        request
            .uri()
            .path_and_query()
            .cloned()
            .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"))
    } else {
        planning::outbound_uri(
            &ctx.original_uri,
            &ctx.request,
            &ctx.edits,
            &route.matcher,
            uri,
        )
        .parse()
        .map_err(|_| error(500, "invalid upstream path"))?
    };
    *request.uri_mut() = http::Uri::builder()
        .scheme(http::uri::Scheme::HTTP)
        .authority(
            target
                .authority(address)
                .map_err(|_| error(500, "invalid upstream authority"))?,
        )
        .path_and_query(path)
        .build()
        .map_err(|_| error(500, "invalid upstream URI"))?;
    strip_hop_headers(request.headers_mut());
    request.headers_mut().insert("host", target.host.clone());
    for (name, value) in &prepared.request_headers {
        let value = value.expand(ctx, &backend.host_header)?;
        if value.is_empty() {
            request.headers_mut().remove(name);
        } else {
            request.headers_mut().insert(name.clone(), value);
        }
    }
    if let Some(trace) = &ctx.trace
        && trace.enabled
        && let Some(span) = &trace.upstream
    {
        span.inject(request.headers_mut());
    }
    let request =
        request.map(|body| RequestBody::new(body, guard.counts.clone(), settings.max_body));
    let mut response = prepared.client.request(request).await.map_err(|failure| {
        if guard.counts.request_error.load(Ordering::Relaxed) == 1 {
            error(413, "request body too large")
        } else if guard.counts.request_error.load(Ordering::Relaxed) == 2 {
            error(400, "incomplete request body").into_down()
        } else {
            upstream_error(
                &failure,
                if failure.is_connect() {
                    FailurePhase::Connect
                } else {
                    FailurePhase::Request
                },
            )
        }
    })?;
    if let Some(span) = ctx.trace.as_mut().and_then(|trace| trace.upstream.as_mut()) {
        span.status = Some(response.status().as_u16());
    }
    listener
        .shared
        .telemetry
        .traffic
        .upstream_headers
        .with_label_values(&[ctx.upstream_label.as_deref().unwrap()])
        .observe(ctx.upstream_started.unwrap().elapsed().as_secs_f64());
    strip_hop_headers(response.headers_mut());
    Ok(response.map(Either::Left))
}

fn reply(status: u16, text: String, location: Option<String>) -> Response<Payload> {
    let mut response = Response::new(Either::Right(Full::new(Bytes::from(text))));
    *response.status_mut() = http::StatusCode::from_u16(status).unwrap();
    response.headers_mut().insert(
        "content-type",
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    use hyper::body::Body;
    if status != 204 && status != 304 {
        let length = response.body().size_hint().exact().unwrap_or(0);
        response
            .headers_mut()
            .insert("content-length", length.into());
    }
    if let Some(location) = location
        && let Ok(value) = location.parse()
    {
        response.headers_mut().insert("location", value);
    }
    response
}

fn strip_hop_headers(headers: &mut http::HeaderMap) {
    let names: Vec<http::HeaderName> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    for name in names {
        headers.remove(name);
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

pub(super) enum FailurePhase {
    Connect,
    Request,
    Response,
}

pub(super) fn upstream_error(
    error: &(dyn std::error::Error + 'static),
    phase: FailurePhase,
) -> Box<pingora::Error> {
    let mut source = Some(error);
    let mut timeout = false;
    while let Some(error) = source {
        timeout |= error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut);
        source = error.source();
    }
    let kind = match (phase, timeout) {
        (FailurePhase::Connect, true) => pingora::ErrorType::ConnectTimedout,
        (FailurePhase::Connect, false) => pingora::ErrorType::ConnectError,
        (FailurePhase::Response, true) => pingora::ErrorType::ReadTimedout,
        // Hyper's request future covers both upload and response headers; do not mislabel writes as reads.
        (FailurePhase::Request, true) => pingora::ErrorType::Custom("UpstreamTimedout"),
        _ => pingora::ErrorType::ReadError,
    };
    pingora::Error::explain(kind, error.to_string()).into_up()
}
