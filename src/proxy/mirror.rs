use super::*;
use bytes::BytesMut;

pub(super) struct MirrorRequest {
    route: Arc<Route>,
    snapshot: Arc<RuntimeSnapshot>,
    telemetry: Arc<crate::telemetry::Telemetry>,
    backend: String,
    timeout: std::time::Duration,
    preserve_host: bool,
    header: RequestHeader,
    body: BytesMut,
    limit: usize,
    trace: Option<crate::otlp::trace::ClientSpan>,
    _global: tokio::sync::OwnedSemaphorePermit,
    _tenant: Option<crate::tenancy::Permit>,
}
impl Proxy {
    pub(super) fn prepare_mirror(
        &self,
        session: &mut Session,
        header: &RequestHeader,
        ctx: &mut Context,
    ) {
        self.prepare_mirror_headers(header, ctx, session.was_upgraded(), session.is_body_empty());
    }
    pub(super) fn prepare_mirror_headers(
        &self,
        header: &RequestHeader,
        ctx: &mut Context,
        upgraded: bool,
        empty: bool,
    ) {
        let Some(route) = &ctx.route else {
            return;
        };
        let (backend, numerator, denominator, limit, timeout, preserve_host) = if let Some(policy) =
            route
                .settings
                .gateway
                .as_ref()
                .and_then(|p| p.mirror.as_ref())
        {
            let Some(backend) = &policy.backend else {
                return;
            };
            (
                backend.clone(),
                policy.numerator,
                policy.denominator,
                64 * 1024,
                std::time::Duration::from_millis(500),
                true,
            )
        } else if let Some(rollout) = &route.rollout {
            let Some(policy) = &rollout.policy.mirror else {
                return;
            };
            let Some(backend) = rollout.backends.get(&policy.service) else {
                return;
            };
            (
                backend.clone(),
                policy.percent,
                100,
                policy.max_body_bytes,
                std::time::Duration::from_millis(policy.timeout_ms),
                false,
            )
        } else {
            return;
        };
        use rand::Rng;
        if rand::thread_rng().gen_range(0..denominator) >= numerator {
            return;
        }
        let skip = upgraded
            || header.headers.contains_key("upgrade")
            || ctx
                .request
                .headers
                .get("content-type")
                .is_some_and(|v| v.starts_with("application/grpc"))
            || header
                .headers
                .get("content-length")
                .and_then(|v| v.to_str().ok()?.parse::<usize>().ok())
                .is_some_and(|n| n > limit);
        let global = self.shared.mirrors.clone().try_acquire_owned();
        let tenant = route
            .tenant
            .as_ref()
            .map(|t| t.acquire(crate::tenancy::Resource::Mirror))
            .transpose();
        if skip || global.is_err() || tenant.is_err() {
            if !skip
                && let Some(namespace) = &route.tenant
                && tenant.is_err()
            {
                self.shared
                    .telemetry
                    .namespace_rejected(&namespace.name, "mirror");
            }
            self.shared
                .telemetry
                .mirror_results
                .with_label_values(&["skipped"])
                .inc();
            return;
        }
        ctx.mirror = Some(Box::new(MirrorRequest {
            route: route.clone(),
            snapshot: ctx.snapshot.as_ref().unwrap().clone(),
            telemetry: self.shared.telemetry.clone(),
            backend: backend.clone(),
            timeout,
            preserve_host,
            header: header.clone(),
            body: BytesMut::new(),
            limit,
            trace: ctx
                .trace
                .as_ref()
                .map(|t| t.client(ctx.request.method.as_str(), &backend, "mirror")),
            _global: global.unwrap(),
            _tenant: tenant.unwrap(),
        }));
        if empty {
            self.send_mirror(ctx);
        }
    }
    pub(super) fn mirror_body(&self, body: &Option<Bytes>, end: bool, ctx: &mut Context) {
        feed(&mut ctx.mirror, body.as_ref(), end);
    }
    fn send_mirror(&self, ctx: &mut Context) {
        if let Some(mirror) = ctx.mirror.take() {
            mirror.send();
        }
    }
}

pub(super) fn feed(mirror: &mut Option<Box<MirrorRequest>>, body: Option<&Bytes>, end: bool) {
    let Some(active) = mirror else {
        return;
    };
    if let Some(bytes) = body {
        if active.body.len() + bytes.len() > active.limit {
            active
                .telemetry
                .mirror_results
                .with_label_values(&["skipped"])
                .inc();
            *mirror = None;
            return;
        }
        active.body.extend_from_slice(bytes);
    }
    if end && let Some(mirror) = mirror.take() {
        mirror.send();
    }
}
impl MirrorRequest {
    fn send(self: Box<Self>) {
        let route = self.route.clone();
        let snapshot = self.snapshot.clone();
        let telemetry = self.telemetry.clone();
        let mut mirror = self;
        tokio::spawn(async move {
            let result = async {
                let backend = snapshot
                    .backends
                    .get(&mirror.backend)
                    .ok_or_else(|| anyhow::anyhow!("mirror backend withdrawn"))?;
                let transport = if route.settings.gateway.is_some() {
                    &backend.profile
                } else {
                    &route.settings.upstream
                };
                let lease = backend
                    .select("")
                    .ok_or_else(|| anyhow::anyhow!("mirror unavailable"))?;
                let name = transport
                    .server_name
                    .as_deref()
                    .unwrap_or(&backend.hostname);
                let host = if name.parse::<std::net::Ipv6Addr>().is_ok() {
                    format!("[{name}]")
                } else {
                    name.into()
                };
                let uri = mirror
                    .header
                    .uri
                    .path_and_query()
                    .map_or("/", |p| p.as_str());
                let url = format!(
                    "{}://{host}:{}{uri}",
                    if backend.tls { "https" } else { "http" },
                    lease.address.port()
                );
                let mut builder = transport
                    .client_builder()?
                    .resolve(name, lease.address)
                    .timeout(mirror.timeout)
                    .pool_max_idle_per_host(0);
                if transport.protocol == crate::upstream::Protocol::Http2 {
                    builder = builder.http2_prior_knowledge();
                }
                let client = builder.build()?;
                let mut headers = mirror.header.headers.clone();
                let host_header = match (mirror.preserve_host, headers.get("host")) {
                    (true, Some(host)) => host.clone(),
                    _ => http::HeaderValue::from_str(&backend.host_header)?,
                };
                for name in [
                    "host",
                    "content-length",
                    "transfer-encoding",
                    "connection",
                    "upgrade",
                    "expect",
                    "trailer",
                ] {
                    headers.remove(name);
                }
                if let Some(span) = &mirror.trace {
                    span.inject(&mut headers);
                }
                let response = client
                    .request(mirror.header.method.clone(), url)
                    .headers(headers)
                    .header("Host", host_header)
                    .header("x-rgnix-mirror", "true")
                    .body(mirror.body.to_vec())
                    .send()
                    .await?;
                if let Some(span) = &mut mirror.trace {
                    span.status = Some(response.status().as_u16());
                }
                anyhow::Ok(())
            }
            .await;
            if let (Some(span), Some(exporter)) = (&mirror.trace, &telemetry.traces) {
                span.export(exporter, result.is_err().then_some("mirror_request"), None);
            }
            telemetry
                .mirror_results
                .with_label_values(&[if result.is_ok() { "sent" } else { "error" }])
                .inc();
            drop(mirror);
        });
    }
}
