mod controller;
mod runtime;
pub(crate) mod traffic;
use anyhow::Result;
use prometheus::{
    Encoder, Histogram, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Registry,
    TextEncoder,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub struct Telemetry {
    pub traffic: traffic::Traffic,
    pub controller: controller::Controller,
    pub config_updated: prometheus::Gauge,
    pub config_update_seconds: HistogramVec,
    pub otlp: Option<crate::otlp::Exporter>,
    pub traces: Option<crate::otlp::Exporter>,
    pub trace_ratio: Option<f64>,
    route_requests: IntCounterVec,
    grpc_requests: IntCounterVec,
    route_duration: HistogramVec,
    backend_requests: IntCounterVec,
    labels: std::sync::Mutex<std::collections::BTreeSet<String>>,
    pub healthy: AtomicBool,
    pub ready: AtomicBool,
    pub registry: Registry,
    pub requests: IntCounterVec,
    pub duration: Histogram,
    pub plugin_errors: IntCounter,
    pub plugin_calls: IntCounter,
    pub plugin_duration: Histogram,
    pub upstream_errors: IntCounter,
    pub reload_errors: IntCounter,
    pub reloads: IntCounter,
    pub version: IntGauge,
    pub dropped_logs: IntCounter,
    pub config_degraded: IntGauge,
    pub checkpoint_healthy: IntGauge,
    pub checkpoint_errors: IntCounter,
    pub report_errors: IntCounter,
    pub config_info: IntGaugeVec,
    pub rejected: IntCounterVec,
    pub upstream_ejections: IntCounter,
    pub rollbacks: IntCounter,
    pub mirror_results: IntCounterVec,
    tenant_rejected: IntCounterVec,
    label_overflow: IntCounterVec,
    pub files: Arc<crate::logging::FileLogs>,
    pub certificate_expiry: IntGaugeVec,
    pub certificate_valid: IntGaugeVec,
    pub control_reloads: IntCounter,
    pub control_reload_errors: IntCounter,
}
impl Telemetry {
    pub fn new(otlp: crate::otlp::Options) -> Result<Arc<Self>> {
        let registry = Registry::new();
        #[cfg(target_os = "linux")]
        registry.register(Box::new(
            prometheus::process_collector::ProcessCollector::for_self(),
        ))?;
        let traffic = traffic::Traffic::new(&registry)?;
        let controller = controller::Controller::new(&registry)?;
        let config_updated = prometheus::register_gauge_with_registry!(
            "rgnix_config_last_success_timestamp_seconds",
            "Last successful snapshot publication, including standalone startup",
            registry
        )?;
        let config_update_seconds = prometheus::register_histogram_vec_with_registry!(
            "rgnix_config_update_seconds",
            "Configuration build and publication duration",
            &["source", "result"],
            registry
        )?;
        let label_overflow = prometheus::register_int_counter_vec_with_registry!(
            "rgnix_metric_label_overflow_total",
            "Metric observations assigned to the overflow label",
            &["kind"],
            registry
        )?;
        let certificate_expiry = IntGaugeVec::new(
            prometheus::Opts::new(
                "rgnix_certificate_expiry_timestamp_seconds",
                "Certificate expiration time",
            ),
            &["listener", "host"],
        )?;
        let certificate_valid = IntGaugeVec::new(
            prometheus::Opts::new(
                "rgnix_certificate_valid",
                "Certificate time and hostname validity",
            ),
            &["listener", "host"],
        )?;
        registry.register(Box::new(certificate_expiry.clone()))?;
        registry.register(Box::new(certificate_valid.clone()))?;
        let control_reloads = IntCounter::new(
            "rgnix_control_reloads_total",
            "Access and tenant policy updates",
        )?;
        let control_reload_errors = IntCounter::new(
            "rgnix_control_reload_errors_total",
            "Rejected access and tenant policy updates",
        )?;
        registry.register(Box::new(control_reloads.clone()))?;
        registry.register(Box::new(control_reload_errors.clone()))?;
        anyhow::ensure!(
            otlp.trace_sample_ratio.is_finite() && (0.0..=1.0).contains(&otlp.trace_sample_ratio),
            "trace sample ratio must be 0..1"
        );
        let traces = crate::otlp::Exporter::start_traces(otlp.clone(), &registry)?;
        let trace_ratio = traces.as_ref().map(|_| otlp.trace_sample_ratio);
        let otlp = crate::otlp::Exporter::start(otlp, &registry)?;
        let grpc_requests = IntCounterVec::new(
            prometheus::Opts::new(
                "rgnix_grpc_requests_total",
                "Completed RPCs by route and gRPC status",
            ),
            &["route", "grpc_status"],
        )?;
        registry.register(Box::new(grpc_requests.clone()))?;
        let route_requests = IntCounterVec::new(
            prometheus::Opts::new(
                "rgnix_route_requests_total",
                "Completed requests by configured route",
            ),
            &["route", "status_class"],
        )?;
        let route_duration = HistogramVec::new(
            prometheus::HistogramOpts::new(
                "rgnix_route_request_seconds",
                "Request duration by configured route",
            ),
            &["route"],
        )?;
        let backend_requests = IntCounterVec::new(
            prometheus::Opts::new(
                "rgnix_backend_requests_total",
                "Completed requests by configured backend",
            ),
            &["backend", "result"],
        )?;
        registry.register(Box::new(route_requests.clone()))?;
        registry.register(Box::new(route_duration.clone()))?;
        registry.register(Box::new(backend_requests.clone()))?;
        let tenant_rejected = IntCounterVec::new(
            prometheus::Opts::new(
                "rgnix_namespace_rejections_total",
                "Requests rejected by namespace budgets",
            ),
            &["namespace", "resource"],
        )?;
        registry.register(Box::new(tenant_rejected.clone()))?;
        let rollbacks = IntCounter::new(
            "rgnix_traffic_rollbacks_total",
            "Automatic traffic rollbacks",
        )?;
        let mirror_results = IntCounterVec::new(
            prometheus::Opts::new("rgnix_mirror_requests_total", "Mirror outcomes"),
            &["result"],
        )?;
        registry.register(Box::new(rollbacks.clone()))?;
        registry.register(Box::new(mirror_results.clone()))?;
        let upstream_ejections = IntCounter::new(
            "rgnix_upstream_ejections_total",
            "Endpoints excluded after repeated transport failures",
        )?;
        registry.register(Box::new(upstream_ejections.clone()))?;
        let config_info = IntGaugeVec::new(
            prometheus::Opts::new("rgnix_config_info", "Active configuration fingerprint"),
            &["sha256"],
        )?;
        let rejected = IntCounterVec::new(
            prometheus::Opts::new(
                "rgnix_rejected_requests_total",
                "Requests rejected by process resource budgets",
            ),
            &["budget"],
        )?;
        registry.register(Box::new(config_info.clone()))?;
        registry.register(Box::new(rejected.clone()))?;
        let requests = IntCounterVec::new(
            prometheus::Opts::new("rgnix_requests_total", "Completed requests"),
            &["status"],
        )?;
        let duration = Histogram::with_opts(prometheus::HistogramOpts::new(
            "rgnix_request_seconds",
            "Request duration",
        ))?;
        let plugin_errors = IntCounter::new(
            "rgnix_plugin_errors_total",
            "Plugin traps or invalid actions",
        )?;
        let plugin_calls = IntCounter::new(
            "rgnix_plugin_calls_total",
            "Request and response plugin evaluations",
        )?;
        let plugin_duration = Histogram::with_opts(
            prometheus::HistogramOpts::new(
                "rgnix_plugin_seconds",
                "Plugin evaluation duration including request instantiation",
            )
            .buckets(vec![
                0.00001, 0.000025, 0.00005, 0.0001, 0.00025, 0.001, 0.01, 0.1,
            ]),
        )?;
        let upstream_errors = IntCounter::new("rgnix_upstream_errors_total", "Upstream errors")?;
        let reload_errors = IntCounter::new(
            "rgnix_reload_errors_total",
            "Rejected configuration updates",
        )?;
        let reloads = IntCounter::new("rgnix_reloads_total", "Published configuration updates")?;
        let version = IntGauge::new("rgnix_config_version", "Current configuration version")?;
        let dropped_logs = IntCounter::new(
            "rgnix_access_logs_dropped_total",
            "Access records dropped by queue overflow or file I/O failure",
        )?;
        let config_degraded = IntGauge::new(
            "rgnix_config_diagnostics",
            "Current configuration diagnostics",
        )?;
        let checkpoint_healthy = IntGauge::new(
            "rgnix_checkpoint_healthy",
            "Last checkpoint persistence succeeded",
        )?;
        let checkpoint_errors = IntCounter::new(
            "rgnix_checkpoint_errors_total",
            "Checkpoint persistence failures",
        )?;
        let report_errors = IntCounter::new(
            "rgnix_report_errors_total",
            "Kubernetes reporting failures or timeouts",
        )?;
        registry.register(Box::new(config_degraded.clone()))?;
        registry.register(Box::new(checkpoint_healthy.clone()))?;
        registry.register(Box::new(requests.clone()))?;
        registry.register(Box::new(duration.clone()))?;
        registry.register(Box::new(plugin_duration.clone()))?;
        for counter in [
            &plugin_errors,
            &plugin_calls,
            &upstream_errors,
            &reload_errors,
            &reloads,
            &dropped_logs,
            &checkpoint_errors,
            &report_errors,
        ] {
            registry.register(Box::new(counter.clone()))?;
        }
        registry.register(Box::new(version.clone()))?;
        let files = crate::logging::FileLogs::start(&registry, dropped_logs.clone())?;
        Ok(Arc::new(Self {
            traffic,
            controller,
            config_updated,
            config_update_seconds,
            label_overflow,
            certificate_expiry,
            certificate_valid,
            control_reloads,
            control_reload_errors,
            otlp,
            traces,
            trace_ratio,
            route_requests,
            grpc_requests,
            route_duration,
            backend_requests,
            labels: Default::default(),
            healthy: AtomicBool::new(true),
            ready: AtomicBool::new(false),
            registry,
            requests,
            duration,
            plugin_errors,
            plugin_calls,
            plugin_duration,
            upstream_errors,
            reload_errors,
            reloads,
            version,
            dropped_logs,
            config_degraded,
            checkpoint_healthy,
            checkpoint_errors,
            report_errors,
            config_info,
            rejected,
            upstream_ejections,
            rollbacks,
            mirror_results,
            tenant_rejected,
            files,
        }))
    }
    pub fn access(&self, path: PathBuf, line: String) {
        self.files.access(path, line);
    }
    pub(crate) fn label(&self, kind: &str, value: &str) -> String {
        let mut labels = self.labels.lock().unwrap_or_else(|e| e.into_inner());
        let key = format!("{kind}:{value}");
        if labels.contains(&key) || labels.len() < 2048 {
            labels.insert(key);
            value.to_owned()
        } else {
            self.label_overflow.with_label_values(&[kind]).inc();
            "_overflow".into()
        }
    }
    pub(crate) fn namespace_rejected(&self, namespace: &str, resource: &str) {
        let namespace = self.label("namespace", namespace);
        self.tenant_rejected
            .with_label_values(&[namespace.as_str(), resource])
            .inc();
    }
    pub(crate) fn observe_runtime(
        shared: &Arc<crate::runtime::Shared>,
        limits: &crate::runtime::Limits,
    ) -> Result<()> {
        shared
            .telemetry
            .registry
            .register(Box::new(runtime::RuntimeCollector::new(shared, limits)?))?;
        Ok(())
    }
    pub fn completed(
        &self,
        route: &str,
        backend: Option<&str>,
        status: u16,
        seconds: f64,
        failed: bool,
        grpc_status: Option<u16>,
    ) {
        let route = self.label("route", route);
        if let Some(code) = grpc_status {
            self.grpc_requests
                .with_label_values(&[&route, &code.to_string()])
                .inc();
        }
        self.route_requests
            .with_label_values(&[&route, &format!("{}xx", status / 100)])
            .inc();
        self.route_duration
            .with_label_values(&[&route])
            .observe(seconds);
        if let Some(backend) = backend {
            let backend = self.label("backend", backend);
            self.backend_requests
                .with_label_values(&[backend.as_str(), if failed { "error" } else { "ok" }])
                .inc();
        }
    }
    pub fn render(&self, path: &str) -> (u16, Vec<u8>, &'static str) {
        match path {
            "/healthz" => {
                if self.healthy.load(Ordering::Acquire) {
                    (200, b"ok\n".to_vec(), "text/plain")
                } else {
                    (503, b"controller failed\n".to_vec(), "text/plain")
                }
            }
            "/readyz" => {
                if self.ready.load(Ordering::Acquire) {
                    (200, b"ready\n".to_vec(), "text/plain")
                } else {
                    (503, b"not ready\n".to_vec(), "text/plain")
                }
            }
            "/metrics" => {
                let mut output = vec![];
                match TextEncoder::new().encode(&self.registry.gather(), &mut output) {
                    Ok(()) => (200, output, "text/plain; version=0.0.4"),
                    Err(_) => (500, vec![], "text/plain"),
                }
            }
            _ => (404, b"not found\n".to_vec(), "text/plain"),
        }
    }
}
