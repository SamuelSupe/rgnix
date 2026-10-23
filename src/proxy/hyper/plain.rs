use super::{
    Listener,
    body::{Counters, Payload, RequestBody, ResponseBody},
    io::Connection,
    prepared::{PreparedTarget, Value},
    request::{FailurePhase, failure_response, strip_hop_headers, upstream_error},
};
use crate::{
    model::{Action as RouteAction, PathMatch, Route, RuntimeSnapshot},
    proxy::{
        Proxy,
        completion::{Completion, UpstreamCompletion},
        error,
    },
};
use bytes::Bytes;
use http::header;
use http_body_util::Full;
use hyper::{Request, Response};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

pub(super) enum Action {
    Proxy(PreparedTarget),
    Reply {
        status: http::StatusCode,
        headers: http::HeaderMap,
        body: Bytes,
    },
}

impl Action {
    pub fn prepare(route: &Route, target: Option<&PreparedTarget>) -> anyhow::Result<Option<Self>> {
        let s = &route.settings;
        // Only policy-free routes can omit the original request view. A later
        // snapshot must re-evaluate this gate before publishing its execution plan.
        if route.script.is_some()
            || route.tenant.is_some()
            || route.rollout.is_some()
            || s.gateway.is_some()
            || s.access_log.is_some()
            || !s.identity.trusted.is_empty()
            || !s.identity.access.is_empty()
            || s.traffic.rate.is_some()
            || s.traffic.concurrency.is_some()
            || s.security.jwt.is_some()
            || s.security.external.is_some()
            || s.security.mtls.mode != crate::security::mtls::Mode::Off
            || s.https_redirect_port.is_some()
            || s.compression.gzip > 0
            || s.compression.brotli > 0
            || s.request_headers.iter().any(|(_, v)| v.contains('$'))
            || s.response_headers.iter().any(|h| h.value.contains('$'))
        {
            return Ok(None);
        }
        Ok(match &route.action {
            RouteAction::Proxy { uri: None, .. }
                if target.is_some_and(|b| {
                    matches!(
                        b.backend.backend.options.balance,
                        crate::backend::Balance::RoundRobin
                            | crate::backend::Balance::LeastConnections
                    ) && s.upstream.protocol == crate::upstream::Protocol::Http1
                }) =>
            {
                Some(Self::Proxy(target.unwrap().clone()))
            }
            RouteAction::Return { status, text } if !text.contains('$') => {
                let redirect = (300..400).contains(status) && !text.is_empty();
                let reply = super::responses::reply(
                    *status,
                    if redirect {
                        String::new()
                    } else {
                        text.clone()
                    },
                    redirect.then(|| text.clone()),
                );
                let (parts, _) = reply.into_parts();
                Some(Self::Reply {
                    status: parts.status,
                    headers: parts.headers,
                    body: if redirect || [204, 304].contains(status) {
                        Bytes::new()
                    } else {
                        Bytes::copy_from_slice(text.as_bytes())
                    },
                })
            }
            _ => None,
        })
    }
}

pub(super) struct Selected {
    snapshot: Arc<RuntimeSnapshot>,
    route: Arc<Route>,
    started: Instant,
}

pub(super) fn select(listener: &Listener, request: &Request<Payload>) -> Option<Selected> {
    if request.version() != http::Version::HTTP_11
        || request.method() == http::Method::CONNECT
        || listener.shared.telemetry.trace_ratio.is_some()
        || request.headers().contains_key("traceparent")
        || request.headers().contains_key(header::UPGRADE)
        || request
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v.as_bytes().starts_with(b"application/grpc"))
    {
        return None;
    }
    let started = Instant::now();
    let snapshot = listener.shared.snapshot.load_full();
    let prepared = snapshot.hyper.as_ref()?;
    if snapshot.gateway.is_some() || !prepared.has_plain {
        return None;
    }
    crate::proxy::validate_connection_header(request.headers()).ok()?;
    let host =
        crate::proxy::request_host_parts(request.headers(), request.uri(), request.version())
            .ok()?;
    let path = crate::proxy::normalized_path_view(request.uri().path()).ok()?;
    let route = snapshot.route(listener.address, &host, &path)?;
    prepared.route(listener.address, &route).plain.as_ref()?;
    if prepared.slash_redirects
        && !path.ends_with('/')
        && !matches!(route.matcher, PathMatch::Exact(_))
    {
        let slash = format!("{path}/");
        if snapshot
            .route(listener.address, &host, &slash)
            .is_some_and(|r| {
                matches!(&r.matcher, PathMatch::NginxPrefix(p) if p == &slash)
                    && matches!(r.action, RouteAction::Proxy { .. })
            })
        {
            return None;
        }
    }
    Some(Selected {
        snapshot,
        route,
        started,
    })
}

pub(super) struct Guard {
    pub proxy: Proxy,
    pub counts: Arc<Counters>,
    pub snapshot: Arc<RuntimeSnapshot>,
    pub route: Arc<Route>,
    pub started: Instant,
    pub _permit: tokio::sync::OwnedSemaphorePermit,
    backend_attempted: bool,
    pub upstream_started: Option<Instant>,
    pub lease: Option<crate::backend::Lease>,
    pub error: Option<Box<pingora::Error>>,
    pub status: u16,
    pub io_failure: Arc<std::sync::atomic::AtomicU8>,
    finished: bool,
}

impl Guard {
    pub fn finish(&mut self, delivered: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        if !delivered && self.error.is_none() {
            let kind = match self.io_failure.load(Ordering::Relaxed) {
                1 => pingora::ErrorType::ReadTimedout,
                2 => pingora::ErrorType::WriteTimedout,
                _ => pingora::ErrorType::ConnectionClosed,
            };
            self.error = Some(
                pingora::Error::explain(kind, "downstream did not complete response").into_down(),
            );
        }
        let backend = match &self.route.action {
            RouteAction::Proxy { backend, .. } => Some(backend.as_str()),
            _ => None,
        };
        let telemetry = &self.proxy.shared.telemetry;
        let metrics = backend.and_then(|name| {
            let Action::Proxy(target) = self
                .snapshot
                .hyper
                .as_ref()?
                .route(self.proxy.listener, &self.route)
                .plain
                .as_ref()?
            else {
                return None;
            };
            target.backend.metrics(telemetry, name)
        });
        let label = backend.map(|b| {
            if metrics.is_some() {
                b
            } else {
                telemetry.label("backend", b)
            }
        });
        let completion = Completion {
            version: http::Version::HTTP_11,
            status: self.status,
            request_bytes: self.counts.request.load(Ordering::Relaxed) as usize,
            response_bytes: self.counts.response.load(Ordering::Relaxed) as usize,
            error: self.error.as_deref(),
        };
        let duration = self.proxy.record_completion(
            self.started,
            self.lease.as_ref().and_then(|lease| {
                Some(UpstreamCompletion {
                    lease,
                    label: label?,
                    duration_metric: metrics.map(|m| &m.duration),
                    started: self.upstream_started?,
                })
            }),
            &completion,
        );
        let upstream_failed = self
            .error
            .as_ref()
            .is_some_and(|e| e.esource() == &pingora::ErrorSource::Upstream);
        if let Some(metrics) = metrics {
            telemetry.route_completed(Some(&self.route), self.status, duration.as_secs_f64(), None);
            if self.backend_attempted {
                metrics.completed(upstream_failed || self.status >= 500);
            }
        } else {
            telemetry.completed(
                Some(&self.route),
                backend.filter(|_| self.backend_attempted),
                self.status,
                duration.as_secs_f64(),
                upstream_failed || self.status >= 500,
                None,
            );
        }
        if let Some(e) = &self.error {
            if upstream_failed {
                self.proxy.shared.telemetry.upstream_errors.inc();
            }
            log::warn!(
                "request failed route={} backend={} upstream={:?} version={}: {e}",
                self.route.id,
                backend.unwrap_or("-"),
                self.lease.as_ref().map(|l| l.address),
                self.snapshot.version
            );
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.finish(false);
    }
}

pub(super) async fn serve(
    listener: &Listener,
    request: Request<Payload>,
    connection: Arc<Connection>,
    selected: Selected,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response<ResponseBody> {
    let mut guard = Box::new(Guard {
        proxy: Proxy {
            shared: listener.shared.clone(),
            listener: listener.address,
            tls: connection.tls,
        },
        counts: Arc::new(Counters::default()),
        snapshot: selected.snapshot,
        route: selected.route,
        started: selected.started,
        _permit: permit,
        backend_attempted: false,
        upstream_started: None,
        lease: None,
        error: None,
        status: 499,
        io_failure: connection.failure.clone(),
        finished: false,
    });
    connection.read_timeout_ms.store(
        guard
            .route
            .settings
            .client_body_timeout
            .as_millis()
            .min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
    connection.write_timeout_ms.store(
        guard
            .route
            .settings
            .send_timeout
            .as_millis()
            .min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
    let head = request.method() == http::Method::HEAD;
    let mut response = match forward(listener, request, &mut guard).await {
        Ok(response) => response,
        Err(failure) => {
            let response = failure_response(&failure);
            guard.error = Some(failure);
            response
        }
    };
    guard.status = response.status().as_u16();
    let prepared = guard
        .snapshot
        .hyper
        .as_ref()
        .unwrap()
        .route(listener.address, &guard.route);
    for (name, value, always) in &prepared.response_headers {
        if (*always || [200, 201, 204, 206, 301, 302, 303, 304, 307, 308].contains(&guard.status))
            && let Value::Constant(value) = value
            && !value.is_empty()
        {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    if head || [204, 304].contains(&guard.status) {
        *response.body_mut() = Payload::Full(Full::new(Bytes::new()));
    }
    if guard.error.is_some()
        || listener.shared.telemetry.draining.get() != 0
        || guard.route.settings.keepalive.is_zero()
    {
        response
            .headers_mut()
            .insert(header::CONNECTION, http::HeaderValue::from_static("close"));
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, ResponseBody::plain(body, guard, connection))
}

async fn forward(
    listener: &Listener,
    mut request: Request<Payload>,
    guard: &mut Guard,
) -> pingora::Result<Response<Payload>> {
    let settings = &guard.route.settings;
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
    let prepared = guard
        .snapshot
        .hyper
        .as_ref()
        .unwrap()
        .route(listener.address, &guard.route);
    let selected = match prepared.plain.as_ref().unwrap() {
        Action::Reply {
            status,
            headers,
            body,
        } => {
            let mut response = Response::new(Payload::Full(Full::new(body.clone())));
            *response.status_mut() = *status;
            *response.headers_mut() = headers.clone();
            return Ok(response);
        }
        Action::Proxy(target) => target,
    };
    let RouteAction::Proxy { backend: name, .. } = &guard.route.action else {
        unreachable!()
    };
    guard.backend_attempted = true;
    let lease = selected
        .backend
        .backend
        .select("")
        .ok_or_else(|| error(503, "no ready endpoint or backend budget exhausted"))?;
    let address = lease.address;
    guard.lease = Some(lease);
    guard.upstream_started = Some(Instant::now());
    let te = request
        .headers()
        .get(header::TE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"trailers"));
    let trailer = request.headers().get(header::TRAILER).cloned();
    strip_hop_headers(request.headers_mut());
    request
        .headers_mut()
        .insert(header::HOST, selected.backend.host.clone());
    for (name, value) in &prepared.request_headers {
        if let Value::Constant(value) = value {
            if value.is_empty() {
                request.headers_mut().remove(name);
            } else {
                request.headers_mut().insert(name.clone(), value.clone());
            }
        }
    }
    if te {
        request
            .headers_mut()
            .insert(header::TE, http::HeaderValue::from_static("trailers"));
    }
    if let Some(trailer) = trailer {
        request.headers_mut().insert(header::TRAILER, trailer);
    }
    let path = request
        .uri()
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"));
    *request.uri_mut() = http::Uri::builder()
        .scheme(if selected.backend.backend.tls {
            "https"
        } else {
            "http"
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
    if let Some(sender) = request
        .extensions_mut()
        .remove::<hyper::ext::SendInformational>()
    {
        hyper::ext::on_informational(&mut request, move |response| {
            let mut interim = Response::new(());
            *interim.status_mut() = response.status();
            *interim.headers_mut() = response.headers().clone();
            strip_hop_headers(interim.headers_mut());
            let _ = sender.try_send(interim);
        });
    }
    let request = request.map(|body| {
        RequestBody::new(
            body,
            guard.counts.clone(),
            settings.max_body,
            settings.client_body_timeout,
        )
    });
    let result = selected.client(listener.worker).request(request).await;
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
    let seconds = guard.upstream_started.unwrap().elapsed().as_secs_f64();
    let telemetry = &listener.shared.telemetry;
    match selected.backend.metrics(telemetry, name) {
        Some(metrics) => metrics.headers.observe(seconds),
        None => telemetry
            .traffic
            .upstream_headers
            .with_label_values(&[telemetry.label("backend", name)])
            .observe(seconds),
    }
    if response.status() == http::StatusCode::SWITCHING_PROTOCOLS {
        return Err(error(502, "unsolicited upstream upgrade"));
    }
    let trailers: Vec<_> = response
        .headers()
        .get_all(header::TRAILER)
        .iter()
        .cloned()
        .collect();
    strip_hop_headers(response.headers_mut());
    for trailer in trailers {
        response.headers_mut().append(header::TRAILER, trailer);
    }
    Ok(response.map(Payload::Incoming))
}
