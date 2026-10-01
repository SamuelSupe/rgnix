use super::{
    Listener,
    body::{Counters, Payload, RequestBody, RequestGuard, ResponseBody},
    io::Connection,
    responses::{self, reply},
};
use crate::{
    model::{Action, PathMatch},
    proxy::{Proxy, error, normalized_path, planning, policy::Dispatch, request_host_parts},
    script,
};
use bytes::Bytes;
use http::header;
use http_body_util::Full;
use hyper::{Request, Response, body::Body};
use pingora::proxy::ProxyHttp;
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

pub(super) async fn serve(
    listener: &Listener,
    request: Request<Payload>,
    peer: SocketAddr,
    connection: Arc<Connection>,
) -> Result<Response<ResponseBody>, Infallible> {
    #[cfg(feature = "http3")]
    let advertise_h3 = request.method() != http::Method::CONNECT
        || request.extensions().get::<hyper::ext::Protocol>().is_some();
    #[cfg(feature = "http3")]
    let public_port = request
        .uri()
        .authority()
        .and_then(|a| a.port_u16())
        .or_else(|| {
            request
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<http::uri::Authority>().ok())
                .and_then(|a| a.port_u16())
        })
        .unwrap_or(443);
    if matches!(
        request.version(),
        http::Version::HTTP_10 | http::Version::HTTP_11
    ) {
        connection.request_active.store(true, Ordering::Relaxed);
    }
    if let Some(selected) = super::plain::select(listener, &request)
        && let Ok(permit) = listener.shared.requests.clone().try_acquire_owned()
    {
        return Ok(super::plain::serve(listener, request, connection, selected, permit).await);
    }
    let proxy = Proxy {
        shared: listener.shared.clone(),
        listener: listener.address,
        tls: connection.tls,
    };
    if matches!(
        request.version(),
        http::Version::HTTP_2 | http::Version::HTTP_3
    ) {
        connection.start_stream();
    }
    let mut guard = Box::new(RequestGuard {
        ctx: proxy.new_ctx(),
        proxy,
        finished: false,
        counts: Arc::new(Counters::default()),
        version: request.version(),
        error: None,
        keepalive: Duration::from_secs(60),
        io_failure: connection.failure.clone(),
        connection: Arc::downgrade(&connection),
        deadline: None,
        upgrade: None,
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
    let result = forward(listener, request, peer, &mut guard, &connection).await;
    let mut response = match result {
        Ok(response) => response,
        Err(failure) => {
            let response = failure_response(&failure);
            guard.error = Some(failure);
            response
        }
    };
    if guard.ctx.request.method == "CONNECT"
        && response.status().is_success()
        && guard.upgrade.is_none()
    {
        guard.error = Some(error(502, "upstream did not establish a CONNECT tunnel"));
        response = reply(
            502,
            "upstream did not establish a CONNECT tunnel".into(),
            None,
        );
    }
    if let Some(route) = &guard.ctx.route {
        guard.keepalive = route.settings.keepalive;
    }
    if let Err(failure) = responses::prepare(&guard.proxy, &mut guard.ctx, &mut response) {
        guard.error = Some(failure);
        response = reply(500, String::new(), None);
        let _ = responses::prepare(&guard.proxy, &mut guard.ctx, &mut response);
    }
    #[cfg(feature = "http3")]
    if advertise_h3
        && connection.tls
        && guard.ctx.snapshot.as_ref().is_some_and(|s| {
            s.hyper
                .as_ref()
                .is_some_and(|p| p.quic.contains_key(&listener.address))
        })
        && !response.headers().contains_key("alt-svc")
    {
        response.headers_mut().insert(
            "alt-svc",
            format!("h3=\":{public_port}\"; ma=60").parse().unwrap(),
        );
    }
    guard.ctx.status = response.status().as_u16();
    if guard.ctx.request.method == "CONNECT"
        && guard.upgrade.is_some()
        && response.status().is_success()
    {
        response.headers_mut().remove(header::CONTENT_LENGTH);
        response.headers_mut().remove(header::TRANSFER_ENCODING);
    }
    if let Some(status) = response.headers().get("grpc-status") {
        guard.ctx.grpc_status = Some(grpc_status(status));
    }
    if guard.ctx.request.method == "HEAD" || [204, 304].contains(&guard.ctx.status) {
        *response.body_mut() = Payload::Full(Full::new(Bytes::new()));
    }
    if matches!(
        guard.version,
        http::Version::HTTP_10 | http::Version::HTTP_11
    ) && response.status() != http::StatusCode::SWITCHING_PROTOCOLS
        && (guard.error.is_some()
            || listener.shared.telemetry.draining.get() != 0
            || guard.keepalive.is_zero())
    {
        response
            .headers_mut()
            .insert(header::CONNECTION, http::HeaderValue::from_static("close"));
    }
    let upgrade = (response.status() == http::StatusCode::SWITCHING_PROTOCOLS
        || response.status().is_success())
    .then(|| guard.upgrade.take())
    .flatten();
    if guard.version == http::Version::HTTP_2 {
        response.extensions_mut().insert(hyper::ext::H2Timeouts {
            read: guard
                .ctx
                .route
                .as_ref()
                .map_or(Duration::from_secs(60), |r| r.settings.client_body_timeout),
            write: guard
                .ctx
                .route
                .as_ref()
                .map_or(Duration::from_secs(60), |r| r.settings.send_timeout),
        });
    }
    #[allow(unused_mut)]
    let (mut parts, body) = response.into_parts();
    #[cfg(target_os = "linux")]
    if let Payload::SendFile(file) = &body {
        parts.extensions.insert(file.clone());
    }
    if let Some(tunnel) = upgrade {
        #[cfg(feature = "http3")]
        if matches!(&tunnel.downstream, super::tunnel::Downstream::H3(_)) {
            let settings = &guard.ctx.route.as_ref().unwrap().settings;
            parts.extensions.insert(super::tunnel::H3Accepted {
                read: settings.client_body_timeout,
                write: settings.send_timeout,
            });
        }
        let state = connection.clone();
        state.set_tunnel(tokio::spawn(super::tunnel::run(tunnel, guard)));
        return Ok(Response::from_parts(
            parts,
            ResponseBody::untracked(body, connection),
        ));
    }
    Ok(Response::from_parts(
        parts,
        ResponseBody::new(body, guard, connection),
    ))
}

async fn forward(
    listener: &Listener,
    request: Request<Payload>,
    peer: SocketAddr,
    guard: &mut RequestGuard,
    connection: &Connection,
) -> pingora::Result<Response<Payload>> {
    let ctx = &mut guard.ctx;
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
    crate::proxy::validate_connection_header(request.headers())?;
    ctx.request.host = request_host_parts(request.headers(), request.uri(), request.version())?;
    ctx.request.path = if request.method() == http::Method::CONNECT
        && request.extensions().get::<hyper::ext::Protocol>().is_none()
    {
        let authority = request
            .uri()
            .authority()
            .ok_or_else(|| error(400, "CONNECT requires an authority"))?;
        if authority.port_u16().is_none() {
            return Err(error(400, "CONNECT requires a numeric port"));
        }
        "/".into()
    } else {
        normalized_path(request.uri().path()).map_err(|e| error(400, e.to_string()))?
    };
    ctx.request.query = request.uri().query().unwrap_or("").into();
    let snapshot = ctx.snapshot.as_ref().unwrap();
    if snapshot.gateway.is_some() {
        ctx.request.headers = request
            .headers()
            .keys()
            .filter_map(|name| {
                request.headers().get(name).map(|v| {
                    (
                        name.as_str().into(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
            })
            .collect();
    }
    ctx.server_name = snapshot
        .hostless_server_name(listener.address)
        .unwrap_or_default()
        .into();
    let Some(mut route) = snapshot.route_request(listener.address, &ctx.request) else {
        ctx.request.headers = script::RequestHeaders::from_http(request.headers());
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
    let prepared = snapshot
        .hyper
        .as_ref()
        .unwrap()
        .route(listener.address, &route);
    if snapshot.gateway.is_none() {
        ctx.request.headers = match &prepared.original_headers {
            Some(names) => script::RequestHeaders::from_selected(request.headers(), names),
            None => script::RequestHeaders::from_http(request.headers()),
        };
    }
    guard.deadline = route
        .settings
        .gateway
        .as_ref()
        .and_then(|p| p.timeouts.request)
        .and_then(|timeout| ctx.started.checked_add(timeout));
    ctx.route = Some(route);
    if !matches!(
        request.version(),
        http::Version::HTTP_2 | http::Version::HTTP_3
    ) {
        let settings = &ctx.route.as_ref().unwrap().settings;
        connection.read_timeout_ms.store(
            settings
                .client_body_timeout
                .as_millis()
                .min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        connection.write_timeout_ms.store(
            settings.send_timeout.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
    let deadline = guard.deadline;
    let future = route_request(listener, request, peer, guard, connection, redirect);
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.into(), future)
            .await
            .map_err(|_| error(504, "request deadline exceeded"))?,
        None => future.await,
    }
}

async fn route_request(
    listener: &Listener,
    mut request: Request<Payload>,
    peer: SocketAddr,
    guard: &mut RequestGuard,
    connection: &Connection,
    redirect: bool,
) -> pingora::Result<Response<Payload>> {
    let ctx = &mut guard.ctx;
    let snapshot = ctx.snapshot.as_ref().unwrap().clone();
    let route = ctx.route.as_ref().unwrap().clone();
    let settings = &route.settings;
    let prepared = snapshot
        .hyper
        .as_ref()
        .unwrap()
        .route(listener.address, &route);
    let client = settings.identity.resolve(
        peer.ip(),
        request.headers(),
        connection.proxy_address.map(|a| a.ip()),
    );
    ctx.request.remote_addr = client.to_string();
    ctx.scheme = settings
        .identity
        .scheme(peer.ip(), request.headers(), connection.tls)
        .into();
    if !settings.identity.allows(client) {
        return Err(error(403, "client address denied"));
    }
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
    let certificate = connection
        .client_certificate
        .as_ref()
        .and_then(|c| c.downcast_ref());
    guard
        .proxy
        .authorize(request.headers(), certificate, ctx)
        .await?;
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
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|n| n > settings.max_body)
    {
        return Err(error(413, "request body too large"));
    }
    let extended = request
        .extensions()
        .get::<hyper::ext::Protocol>()
        .map(|p| p.as_str().to_owned());
    if request.version() == http::Version::HTTP_3
        && request.method() == http::Method::CONNECT
        && request.headers().contains_key(header::CONTENT_LENGTH)
    {
        return Err(error(400, "CONNECT must not carry Content-Length"));
    }
    if extended.as_deref().is_some_and(|p| p != "websocket") {
        return Err(error(501, "unsupported CONNECT protocol"));
    }
    let raw_connect = request.method() == http::Method::CONNECT && extended.is_none();
    if raw_connect && !settings.connect_tunnel {
        return Err(error(405, "CONNECT is disabled for this route"));
    }
    if raw_connect && settings.body_policy.inspection != crate::body::Inspection::Off {
        return Err(error(400, "CONNECT has no inspectable HTTP request body"));
    }
    let websocket = extended.is_some()
        || request
            .headers()
            .get(header::UPGRADE)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    if request.headers().contains_key(header::UPGRADE) && !websocket {
        return Err(error(501, "unsupported upgrade protocol"));
    }
    let valid_websocket = !websocket
        || (extended.is_some()
            && request.method() == http::Method::CONNECT
            && matches!(
                request.version(),
                http::Version::HTTP_2 | http::Version::HTTP_3
            ))
        || !(request.method() != http::Method::GET
            || request.version() != http::Version::HTTP_11
            || !request
                .headers()
                .get_all(header::CONNECTION)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .any(|v| v.trim().eq_ignore_ascii_case("upgrade")));
    let downstream_upgrade = if websocket || raw_connect {
        Some(super::tunnel::downstream(&mut request)?)
    } else {
        None
    };
    let te_trailers = request
        .headers()
        .get(header::TE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"trailers"));
    let trailer = request.headers().get(header::TRAILER).cloned();
    let mut request = request.map(|body| {
        RequestBody::new(
            body,
            guard.counts.clone(),
            settings.max_body,
            settings.client_body_timeout,
        )
    });
    if websocket && !request.body().is_end_stream() {
        request.body_mut().reject_upgrade_body().await?;
    }
    if !valid_websocket {
        return Err(error(400, "invalid WebSocket upgrade"));
    }
    ctx.rollout_stage = route.rollout.as_ref().map_or(0, |r| r.stage());
    if route.script.is_some() {
        guard.proxy.acquire_plugin(ctx)?;
        let started = Instant::now();
        let inspected = request.body_mut().inspect(settings.body_policy).await;
        guard.proxy.observe_inspection(ctx, started, &inspected);
        if let Some((status, body)) = guard.proxy.request_plugin(ctx, inspected?)? {
            return Ok(reply(status, body, None));
        }
        if settings.body_policy.inspection != crate::body::Inspection::Off {
            request.headers_mut().remove(header::EXPECT);
        }
    }
    match guard.proxy.dispatch(ctx)? {
        Dispatch::Reply(status, body, location) => return Ok(reply(status, body, location)),
        Dispatch::Static(route) => {
            let sendfile = cfg!(target_os = "linux")
                && request.version() == http::Version::HTTP_11
                && !connection.tls
                && settings.compression.gzip == 0
                && settings.compression.brotli == 0;
            return responses::file(ctx, route, sendfile).await;
        }
        Dispatch::Proxy => {}
    }
    let backend = guard.proxy.select_endpoint(ctx)?;
    let name = ctx.backend.as_ref().unwrap();
    let selected = prepared
        .targets
        .get(name)
        .ok_or_else(|| error(503, "backend unavailable"))?;
    let address = ctx.upstream_address.unwrap();
    if raw_connect {
        let authority = request
            .uri()
            .authority()
            .ok_or_else(|| error(400, "CONNECT authority missing"))?;
        let target = authority.host().trim_matches(['[', ']']);
        let public = backend
            .host_header
            .parse::<http::uri::Authority>()
            .map_err(|_| error(503, "invalid configured backend authority"))?;
        let configured = target.eq_ignore_ascii_case(&backend.hostname)
            && authority.port_u16()
                == Some(
                    public
                        .port_u16()
                        .unwrap_or(if backend.tls { 443 } else { 80 }),
                );
        let endpoint =
            target == address.ip().to_string() && authority.port_u16() == Some(address.port());
        if !configured && !endpoint {
            return Err(error(403, "CONNECT target is not the configured backend"));
        }
        let started = Instant::now();
        let upstream = tokio::time::timeout(
            settings.connect_timeout,
            tokio::net::TcpStream::connect(address),
        )
        .await
        .map_err(|_| error(504, "CONNECT upstream timed out"))?
        .map_err(|e| error(502, format!("CONNECT upstream: {e}")))?;
        upstream
            .set_nodelay(true)
            .map_err(|e| error(502, e.to_string()))?;
        listener
            .shared
            .telemetry
            .traffic
            .upstream_connect
            .with_label_values(&[ctx.upstream_label.as_deref().unwrap(), "false"])
            .observe(started.elapsed().as_secs_f64());
        guard.upgrade = Some(super::tunnel::Tunnel {
            downstream: downstream_upgrade.unwrap(),
            upstream: super::tunnel::Peer::Tcp(upstream),
        });
        return Ok(reply(200, String::new(), None));
    }
    if route.script.is_some() || settings.gateway.is_some() || settings.security.external.is_some()
    {
        let (parts, body) = request.into_parts();
        let mut header = pingora::http::RequestHeader::from(parts);
        guard.proxy.upstream_headers(&mut header, ctx)?;
        request = Request::from_parts(header.as_owned_parts(), body);
    } else {
        if ctx.uri.is_some() {
            let path = planning::outbound_uri(
                &ctx.original_uri,
                &ctx.request,
                &ctx.edits,
                &route.matcher,
                ctx.uri.as_deref(),
            );
            *request.uri_mut() = path
                .parse()
                .map_err(|_| error(500, "invalid upstream path"))?;
        }
        strip_hop_headers(request.headers_mut());
        request
            .headers_mut()
            .insert(header::HOST, selected.backend.host.clone());
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
    }
    let path = request
        .uri()
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"));
    *request.uri_mut() = http::Uri::builder()
        .scheme(if backend.tls {
            http::uri::Scheme::HTTPS
        } else {
            http::uri::Scheme::HTTP
        })
        .authority(
            selected
                .backend
                .authority(address)
                .map_err(|_| error(500, "invalid upstream authority"))?,
        )
        .path_and_query(path)
        .build()
        .map_err(|_| error(500, "invalid upstream URI"))?;
    let transport = if settings.gateway.is_some() {
        &backend.profile
    } else {
        &settings.upstream
    };
    *request.version_mut() = if transport.protocol == crate::upstream::Protocol::Http2 {
        http::Version::HTTP_2
    } else {
        http::Version::HTTP_11
    };
    if te_trailers {
        request
            .headers_mut()
            .insert(header::TE, http::HeaderValue::from_static("trailers"));
    }
    if let Some(trailer) = trailer {
        request.headers_mut().insert(header::TRAILER, trailer);
    }
    let handshake = if websocket {
        Some(super::websocket::Handshake::prepare(
            &mut request,
            extended.is_some(),
            &transport.protocol,
        )?)
    } else {
        None
    };
    if settings
        .gateway
        .as_ref()
        .is_some_and(|p| p.mirror.is_some())
        || route
            .rollout
            .as_ref()
            .is_some_and(|p| p.policy.mirror.is_some())
    {
        let empty = request.body().is_end_stream();
        let (parts, mut body) = request.into_parts();
        let header = pingora::http::RequestHeader::from(parts);
        guard
            .proxy
            .prepare_mirror_headers(&header, ctx, websocket, empty);
        body.set_mirror(ctx.mirror.take());
        request = Request::from_parts(header.as_owned_parts(), body);
    }
    if let Some(sender) = request
        .extensions_mut()
        .remove::<hyper::ext::SendInformational>()
    {
        let timeouts = hyper::ext::H2Timeouts {
            read: settings.client_body_timeout,
            write: settings.send_timeout,
        };
        hyper::ext::on_informational(&mut request, move |response| {
            let mut interim = Response::new(());
            *interim.status_mut() = response.status();
            *interim.headers_mut() = response.headers().clone();
            strip_hop_headers(interim.headers_mut());
            interim.extensions_mut().insert(timeouts);
            let _ = sender.try_send(interim);
        });
    }
    if transport.protocol != crate::upstream::Protocol::Http1 {
        if let Some(host) = request.headers().get(header::HOST) {
            let authority = host
                .to_str()
                .ok()
                .and_then(|h| h.parse().ok())
                .ok_or_else(|| error(502, "invalid upstream HTTP/2 authority"))?;
            request
                .extensions_mut()
                .insert(hyper_util::client::legacy::RequestAuthority(authority));
        }
        request.extensions_mut().insert(hyper::ext::H2Timeouts {
            read: settings.read_timeout,
            write: settings.write_timeout,
        });
    }
    // Auto WebSocket upgrades require an HTTP/1 pool, rather than sending
    // HTTP/1 framing over a connection that might negotiate HTTP/2 via ALPN.
    let client = if websocket {
        selected.websocket_client(listener.worker)
    } else {
        selected.client(listener.worker)
    };
    let future = client.request(request);
    let result = match settings
        .gateway
        .as_ref()
        .and_then(|p| p.timeouts.backend_request)
    {
        Some(timeout) => tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| error(504, "backend request deadline exceeded"))?,
        None => future.await,
    };
    let mut response =
        result.map_err(
            |failure| match guard.counts.request_error.load(Ordering::Relaxed) {
                1 => error(413, "request body too large"),
                2 => error(400, "incomplete request body").into_down(),
                3 => error(408, "request body timed out").into_down(),
                _ => upstream_error(
                    &failure,
                    if failure.is_connect() {
                        FailurePhase::Connect
                    } else {
                        FailurePhase::Request
                    },
                ),
            },
        )?;
    if let Some(span) = ctx.trace.as_mut().and_then(|t| t.upstream.as_mut()) {
        span.status = Some(response.status().as_u16());
    }
    listener
        .shared
        .telemetry
        .traffic
        .upstream_headers
        .with_label_values(&[ctx.upstream_label.as_deref().unwrap()])
        .observe(ctx.upstream_started.unwrap().elapsed().as_secs_f64());
    let upstream_websocket = match &handshake {
        Some(handshake) => handshake.accept(&mut response)?,
        None => false,
    };
    if upstream_websocket {
        guard.upgrade = Some(super::tunnel::Tunnel {
            downstream: downstream_upgrade
                .ok_or_else(|| error(502, "unsolicited upstream upgrade"))?,
            upstream: super::tunnel::Peer::Upgrade(hyper::upgrade::on(&mut response)),
        });
    } else {
        if response.status() == http::StatusCode::SWITCHING_PROTOCOLS {
            return Err(error(502, "unsolicited upstream upgrade"));
        }
        let trailers = response
            .headers()
            .get_all(header::TRAILER)
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        strip_hop_headers(response.headers_mut());
        for trailer in trailers {
            response.headers_mut().append(header::TRAILER, trailer);
        }
    }
    Ok(response.map(Payload::Incoming))
}

pub(super) fn grpc_status(value: &http::HeaderValue) -> u16 {
    value
        .to_str()
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|v| *v <= 16)
        .unwrap_or(2)
}

pub(super) fn strip_hop_headers(headers: &mut http::HeaderMap) {
    static HOP_HEADERS: [http::HeaderName; 9] = [
        header::CONNECTION,
        http::HeaderName::from_static("proxy-connection"),
        http::HeaderName::from_static("keep-alive"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ];
    let names: Vec<http::HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|v| {
            !HOP_HEADERS
                .iter()
                .any(|name| v.eq_ignore_ascii_case(name.as_str()))
        })
        .filter_map(|v| v.parse().ok())
        .collect();
    for name in names {
        headers.remove(name);
    }
    for name in &HOP_HEADERS {
        headers.remove(name);
    }
}

pub(super) fn failure_response(failure: &pingora::Error) -> Response<Payload> {
    let status = match failure.etype() {
        pingora::ErrorType::HTTPStatus(status) => *status,
        pingora::ErrorType::Custom("UpstreamTimedout")
        | pingora::ErrorType::ConnectTimedout
        | pingora::ErrorType::ReadTimedout
        | pingora::ErrorType::WriteTimedout => 504,
        _ => 502,
    };
    let mut response = reply(status, String::new(), None);
    if status == 401 {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            http::HeaderValue::from_static("Bearer"),
        );
    }
    if status == 429 || status == 503 {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, http::HeaderValue::from_static("1"));
    }
    response
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
