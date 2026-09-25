use super::*;
use bytes::BytesMut;

pub(super) struct MirrorRequest {
    header: RequestHeader,
    body: BytesMut,
    limit: usize,
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
        let Some(route) = &ctx.route else {
            return;
        };
        let Some(policy) = route
            .rollout
            .as_ref()
            .and_then(|r| r.policy.mirror.as_ref())
        else {
            return;
        };
        if rand::random::<u32>() % 100 >= policy.percent {
            return;
        }
        let skip = session.was_upgraded()
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
                .is_some_and(|n| n > policy.max_body_bytes);
        let global = self.shared.mirrors.clone().try_acquire_owned();
        let tenant = route
            .tenant
            .as_ref()
            .map(|t| t.acquire(crate::tenancy::Resource::Mirror))
            .transpose();
        if skip || global.is_err() || tenant.is_err() {
            self.shared
                .telemetry
                .mirror_results
                .with_label_values(&["skipped"])
                .inc();
            return;
        }
        ctx.mirror = Some(MirrorRequest {
            header: header.clone(),
            body: BytesMut::new(),
            limit: policy.max_body_bytes,
            _global: global.unwrap(),
            _tenant: tenant.unwrap(),
        });
        if session.is_body_empty() {
            self.send_mirror(ctx);
        }
    }
    pub(super) fn mirror_body(&self, body: &Option<Bytes>, end: bool, ctx: &mut Context) {
        let Some(mirror) = &mut ctx.mirror else {
            return;
        };
        if let Some(bytes) = body {
            if mirror.body.len() + bytes.len() > mirror.limit {
                ctx.mirror = None;
                self.shared
                    .telemetry
                    .mirror_results
                    .with_label_values(&["skipped"])
                    .inc();
                return;
            }
            mirror.body.extend_from_slice(bytes);
        }
        if end {
            self.send_mirror(ctx);
        }
    }
    fn send_mirror(&self, ctx: &mut Context) {
        let Some(mirror) = ctx.mirror.take() else {
            return;
        };
        let route = ctx.route.as_ref().unwrap().clone();
        let snapshot = ctx.snapshot.as_ref().unwrap().clone();
        let telemetry = self.shared.telemetry.clone();
        tokio::spawn(async move {
            let result = async {
                let rollout = route.rollout.as_ref().unwrap();
                let policy = rollout.policy.mirror.as_ref().unwrap();
                let backend = &snapshot.backends[&rollout.backends[&policy.service]];
                let lease = backend
                    .select("")
                    .ok_or_else(|| anyhow::anyhow!("mirror unavailable"))?;
                let name = route
                    .settings
                    .upstream
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
                let mut builder = route
                    .settings
                    .upstream
                    .client_builder()?
                    .resolve(name, lease.address)
                    .timeout(std::time::Duration::from_millis(policy.timeout_ms))
                    .pool_max_idle_per_host(0);
                if route.settings.upstream.protocol == crate::upstream::Protocol::Http2 {
                    builder = builder.http2_prior_knowledge();
                }
                let client = builder.build()?;
                let mut headers = mirror.header.headers.clone();
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
                client
                    .request(mirror.header.method.clone(), url)
                    .headers(headers)
                    .header("Host", &backend.host_header)
                    .header("x-rgnix-mirror", "true")
                    .body(mirror.body.to_vec())
                    .send()
                    .await?;
                anyhow::Ok(())
            }
            .await;
            telemetry
                .mirror_results
                .with_label_values(&[if result.is_ok() { "sent" } else { "error" }])
                .inc();
            drop(mirror);
        });
    }
}
