use super::*;

pub(super) struct Completion<'a> {
    pub version: http::Version,
    pub status: u16,
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub error: Option<&'a pingora::Error>,
}

pub(super) struct UpstreamCompletion<'a> {
    pub lease: &'a crate::backend::Lease,
    pub label: &'a str,
    pub duration_metric: Option<&'a prometheus::Histogram>,
    pub started: Instant,
}

impl Proxy {
    pub(super) fn record_completion(
        &self,
        started: Instant,
        upstream: Option<UpstreamCompletion<'_>>,
        completion: &Completion<'_>,
    ) -> std::time::Duration {
        let finished = Instant::now();
        let duration = finished.saturating_duration_since(started);
        let status = completion.status;
        let request_bytes = completion.request_bytes;
        let response_bytes = completion.response_bytes;
        let e = completion.error;
        let traffic = &self.shared.telemetry.traffic;
        traffic.request_bytes.inc_by(request_bytes as u64);
        traffic.response_bytes.inc_by(response_bytes as u64);
        if let Some(error) = e {
            traffic.failure(error);
        }
        if let Some(upstream) = &upstream {
            let duration = finished
                .saturating_duration_since(upstream.started)
                .as_secs_f64();
            match upstream.duration_metric {
                Some(metric) => metric.observe(duration),
                None => traffic
                    .upstream_duration
                    .with_label_values(&[upstream.label])
                    .observe(duration),
            }
        }
        if let Some(upstream) = &upstream
            && (e.is_none()
                || e.is_some_and(|error| error.esource() == &pingora::ErrorSource::Upstream))
            && upstream.lease.record_result(
                e.is_some(),
                self.shared.upstream_max_fails,
                self.shared.upstream_fail_timeout,
            )
        {
            self.shared.telemetry.upstream_ejections.inc();
            log::warn!(
                "temporarily excluding upstream {} after repeated transport failures",
                upstream.lease.address
            );
        }
        self.shared.telemetry.request_completed(status);
        self.shared
            .telemetry
            .duration
            .observe(duration.as_secs_f64());
        duration
    }

    pub(super) fn complete(&self, ctx: &mut Context, completion: Completion<'_>) {
        let Completion {
            version,
            status,
            request_bytes,
            response_bytes,
            error: e,
        } = completion;
        let duration = self.record_completion(
            ctx.started,
            ctx.upstream_lease.as_ref().and_then(|lease| {
                Some(UpstreamCompletion {
                    lease,
                    label: ctx.upstream_label.as_deref()?,
                    duration_metric: None,
                    started: ctx.upstream_started?,
                })
            }),
            &Completion {
                version,
                status,
                request_bytes,
                response_bytes,
                error: e,
            },
        );
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
                duration,
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
            ctx.route.as_deref(),
            ctx.backend.as_deref(),
            status,
            duration.as_secs_f64(),
            upstream_failed || status >= 500 || grpc_status.is_some_and(|s| s != 0),
            grpc_status,
        );
        if let (Some(trace), Some(exporter)) = (&ctx.trace, &self.shared.telemetry.traces) {
            trace.export(
                exporter,
                (route_id, ctx.route.as_ref().map(|r| r.matcher.path())),
                &ctx.request.method,
                (status, grpc_status),
                e,
            );
        }
        if let Some(e) = e {
            if e.esource() == &pingora::ErrorSource::Upstream {
                self.shared.telemetry.upstream_errors.inc();
            }
            log::warn!(
                "request failed route={} backend={} upstream={:?} version={} trace_id={} span_id={}: {e}",
                ctx.route.as_ref().map_or("-", |r| r.id.as_str()),
                ctx.backend.as_deref().unwrap_or("-"),
                ctx.upstream_address,
                ctx.snapshot.as_ref().map_or(0, |s| s.version),
                ctx.trace
                    .as_ref()
                    .map_or_else(|| "-".into(), |t| crate::otlp::trace::hex(&t.trace_id)),
                ctx.trace
                    .as_ref()
                    .map_or_else(|| "-".into(), |t| crate::otlp::trace::hex(&t.span_id))
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
                    trace: ctx.trace.as_deref(),
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
                    protocol_version: match version {
                        http::Version::HTTP_09 => "0.9",
                        http::Version::HTTP_10 => "1.0",
                        http::Version::HTTP_11 => "1.1",
                        http::Version::HTTP_2 => "2",
                        http::Version::HTTP_3 => "3",
                        _ => "unknown",
                    },
                    tls: self.tls,
                    status,
                    response_bytes,
                    duration,
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
                "protocol":format!("{:?}",version),"status":status,"bytes":response_bytes,
                "referer":ctx.request.headers.get("referer").unwrap_or("-"),
                "user_agent":ctx.request.headers.get("user-agent").unwrap_or("-"),
                "route":route_id,"backend":ctx.backend.as_deref().unwrap_or("-"),
                "upstream":ctx.upstream_address.map_or_else(||"-".into(),|a|a.to_string()),
                "config":ctx.snapshot.as_ref().map_or("-",|s|s.content_hash.as_str()),
                "trace_id":ctx.trace.as_ref().map_or_else(||"-".into(),|t|crate::otlp::trace::hex(&t.trace_id)),
                "span_id":ctx.trace.as_ref().map_or_else(||"-".into(),|t|crate::otlp::trace::hex(&t.span_id)),
                "parent_span_id":ctx.trace.as_ref().filter(|t|t.enabled && t.parent != [0;8]).map_or_else(||"-".into(),|t|crate::otlp::trace::hex(&t.parent)),
                "upstream_span_id":ctx.trace.as_ref().filter(|t|t.enabled).and_then(|t|t.upstream.as_ref()).map_or_else(||"-".into(),|s|s.id()),
                "trace_sampled":ctx.trace.as_ref().is_some_and(|t|t.sampled),
                "grpc_status":grpc_status
            }));
            self.shared.telemetry.access(path, line);
        }
    }
}
