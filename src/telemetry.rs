use anyhow::Result;
use prometheus::{
    Encoder, Histogram, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Registry, TextEncoder,
};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
};

pub struct Telemetry {
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
    log: SyncSender<(PathBuf, String)>,
}
impl Telemetry {
    pub fn new() -> Result<Arc<Self>> {
        let registry = Registry::new();
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
            "Access log queue overflow",
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
        let (tx, rx) = sync_channel::<(PathBuf, String)>(4096);
        std::thread::Builder::new()
            .name("access-log".into())
            .spawn(move || {
                let mut files = HashMap::new();
                while let Ok((path, line)) = rx.recv() {
                    if files.len() > 128 {
                        files.clear();
                    }
                    if !files.contains_key(&path) {
                        match OpenOptions::new().create(true).append(true).open(&path) {
                            Ok(file) => {
                                files.insert(path.clone(), file);
                            }
                            Err(e) => {
                                log::error!("access log {}: {e}", path.display());
                                continue;
                            }
                        }
                    }
                    if let Some(file) = files.get_mut(&path)
                        && let Err(e) = writeln!(file, "{line}")
                    {
                        log::error!("access log: {e}");
                        files.remove(&path);
                    }
                }
            })?;
        Ok(Arc::new(Self {
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
            log: tx,
        }))
    }
    pub fn access(&self, path: PathBuf, line: String) {
        if self.log.try_send((path, line)).is_err() {
            self.dropped_logs.inc();
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
