use crate::runtime::{Limits, Shared};
use anyhow::Result;
use prometheus::{
    GaugeVec, Opts,
    core::{Collector, Desc},
    proto::MetricFamily,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak, atomic::Ordering},
};

const LIMIT: usize = 2048;
const GAUGES: &[(&str, &str, &[&str])] = &[
    (
        "rgnix_build_info",
        "Binary and runtime identity",
        &["version", "mode", "arch", "os"],
    ),
    ("rgnix_healthy", "Controller health, matching healthz", &[]),
    ("rgnix_ready", "Readiness, matching readyz", &[]),
    (
        "rgnix_config_resources",
        "Entries in the active runtime snapshot",
        &["kind"],
    ),
    (
        "rgnix_budget_in_use",
        "Occupied process resource permits",
        &["budget"],
    ),
    (
        "rgnix_budget_limit",
        "Process resource permit limits",
        &["budget"],
    ),
    (
        "rgnix_backend_endpoints",
        "Current backend endpoints; health states may overlap",
        &["backend", "state"],
    ),
    (
        "rgnix_backend_inflight",
        "Outstanding requests holding a backend lease",
        &["backend"],
    ),
    (
        "rgnix_backend_inflight_limit",
        "Backend concurrency limit; zero means unlimited",
        &["backend"],
    ),
    (
        "rgnix_backend_dns_age_seconds",
        "Age of the last successful DNS resolution, including initial configuration",
        &["backend"],
    ),
    (
        "rgnix_namespace_budget_in_use",
        "Occupied namespace permits retained by runtime snapshots",
        &["tenant_namespace", "resource"],
    ),
    (
        "rgnix_namespace_budget_limit",
        "Effective namespace resource limits",
        &["tenant_namespace", "resource"],
    ),
    (
        "rgnix_limiter_entries",
        "Allocated rate and concurrency limiter entries",
        &["tenant_namespace"],
    ),
    (
        "rgnix_limiter_capacity",
        "Maximum rate and concurrency limiter entries",
        &["tenant_namespace"],
    ),
    (
        "rgnix_rollout_stage",
        "Current zero-based rollout stage",
        &["owner"],
    ),
    (
        "rgnix_rollout_state",
        "Current rollout state flags",
        &["owner", "state"],
    ),
    (
        "rgnix_metric_gate_passed",
        "External metric gate passed and is fresh for the active controls",
        &["gate"],
    ),
    (
        "rgnix_metric_gate_checked_timestamp_seconds",
        "Last gate check for the active controls; zero if never checked",
        &["gate"],
    ),
    (
        "rgnix_ingress_leader",
        "Last confirmed local Lease claim is still within its lifetime",
        &[],
    ),
    (
        "rgnix_metric_labels",
        "Retained request metric label identities",
        &[],
    ),
    (
        "rgnix_metric_label_limit",
        "Maximum retained request metric label identities",
        &[],
    ),
    (
        "rgnix_metrics_omitted",
        "Current objects beyond the per-category exposition limit",
        &["kind"],
    ),
];

fn gauges() -> Result<BTreeMap<&'static str, GaugeVec>> {
    GAUGES
        .iter()
        .map(|(name, help, labels)| Ok((*name, GaugeVec::new(Opts::new(*name, *help), labels)?)))
        .collect()
}

pub struct RuntimeCollector {
    shared: Weak<Shared>,
    limits: [usize; 4],
    descs: Vec<Desc>,
}
impl RuntimeCollector {
    pub fn new(shared: &Arc<Shared>, limits: &Limits) -> Result<Self> {
        Ok(Self {
            shared: Arc::downgrade(shared),
            limits: [limits.max_inflight, limits.max_plugin_instances, 16, 2],
            descs: gauges()?
                .values()
                .flat_map(|g| g.desc().into_iter().cloned())
                .collect(),
        })
    }
}
impl Collector for RuntimeCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.descs.iter().collect()
    }
    fn collect(&self) -> Vec<MetricFamily> {
        let Some(shared) = self.shared.upgrade() else {
            return vec![];
        };
        // Each scrape owns its samples: deleted objects disappear and concurrent
        // scrapes cannot observe another scrape's partially reset vectors.
        let metrics = gauges().expect("validated metric descriptors");
        let set = |name: &str, labels: &[&str], value: f64| {
            metrics[name].with_label_values(labels).set(value);
        };
        let snapshot = shared.snapshot.load_full();
        set(
            "rgnix_build_info",
            &[
                env!("CARGO_PKG_VERSION"),
                if shared.file_mode {
                    "standalone"
                } else {
                    "ingress"
                },
                std::env::consts::ARCH,
                std::env::consts::OS,
            ],
            1.0,
        );
        set(
            "rgnix_healthy",
            &[],
            u8::from(shared.telemetry.healthy.load(Ordering::Acquire)) as f64,
        );
        set(
            "rgnix_ready",
            &[],
            u8::from(shared.telemetry.ready.load(Ordering::Acquire)) as f64,
        );
        for (kind, count) in [
            ("listeners", snapshot.listeners.len()),
            ("hosts", snapshot.hosts.len()),
            (
                "routes",
                snapshot.hosts.iter().map(|h| h.routes.len()).sum(),
            ),
            ("backends", snapshot.backends.len()),
            ("certificates", snapshot.certificates.len()),
            (
                "script_routes",
                snapshot
                    .hosts
                    .iter()
                    .flat_map(|h| &h.routes)
                    .filter(|r| r.script.is_some())
                    .count(),
            ),
        ] {
            set("rgnix_config_resources", &[kind], count as f64);
        }
        for ((budget, semaphore), limit) in [
            ("inflight", &shared.requests),
            ("plugin", &shared.plugins),
            ("mirror", &shared.mirrors),
            ("simulation", &shared.simulations),
        ]
        .into_iter()
        .zip(self.limits)
        {
            set(
                "rgnix_budget_in_use",
                &[budget],
                limit.saturating_sub(semaphore.available_permits()) as f64,
            );
            set("rgnix_budget_limit", &[budget], limit as f64);
        }
        for (name, backend) in snapshot.backends.iter().take(LIMIT) {
            let value = backend.observation();
            for (state, count) in [
                ("total", value.endpoints),
                ("eligible", value.eligible),
                ("ejected", value.ejected),
                ("unready", value.unready),
            ] {
                set("rgnix_backend_endpoints", &[name, state], count as f64);
            }
            set("rgnix_backend_inflight", &[name], value.active as f64);
            set(
                "rgnix_backend_inflight_limit",
                &[name],
                backend.options.max_inflight as f64,
            );
            if let Some(age) = value.dns_age {
                set("rgnix_backend_dns_age_seconds", &[name], age);
            }
        }
        set(
            "rgnix_metrics_omitted",
            &["backends"],
            snapshot.backends.len().saturating_sub(LIMIT) as f64,
        );
        let (tenant_count, tenants) = shared.tenants.observations(LIMIT);
        for tenant in tenants {
            let quota = tenant.quota.load();
            for ((resource, active), limit) in ["request", "plugin", "auth", "mirror"]
                .into_iter()
                .zip(tenant.active())
                .zip([
                    quota.max_inflight,
                    quota.max_plugins,
                    quota.max_auth,
                    quota.max_mirrors,
                ])
            {
                set(
                    "rgnix_namespace_budget_in_use",
                    &[&tenant.name, resource],
                    active as f64,
                );
                set(
                    "rgnix_namespace_budget_limit",
                    &[&tenant.name, resource],
                    limit as f64,
                );
            }
            let (entries, capacity) = tenant.traffic.observation();
            set("rgnix_limiter_entries", &[&tenant.name], entries as f64);
            set("rgnix_limiter_capacity", &[&tenant.name], capacity as f64);
        }
        set(
            "rgnix_metrics_omitted",
            &["namespaces"],
            tenant_count.saturating_sub(LIMIT) as f64,
        );
        if shared.file_mode {
            let (entries, capacity) = shared.traffic.observation();
            set("rgnix_limiter_entries", &["_standalone"], entries as f64);
            set("rgnix_limiter_capacity", &["_standalone"], capacity as f64);
        }
        let rollouts: BTreeMap<_, _> = snapshot
            .hosts
            .iter()
            .flat_map(|h| &h.routes)
            .filter_map(|r| r.rollout.as_ref())
            .map(|s| {
                (
                    if s.kind == "Ingress" {
                        s.owner.clone()
                    } else {
                        format!("{}:{}", s.kind, s.owner)
                    },
                    s,
                )
            })
            .collect();
        for (owner, state) in rollouts.iter().take(LIMIT) {
            let progress = state.progress();
            let waiting = state
                .policy
                .steps
                .get(progress.stage)
                .is_some_and(|s| s.approval)
                && !progress.promoted
                && !state.rolled_back()
                && progress.approved_stage != Some(progress.stage);
            set("rgnix_rollout_stage", &[owner], progress.stage as f64);
            for (name, enabled) in [
                ("paused", progress.paused),
                ("promoted", progress.promoted),
                ("rolled_back", state.rolled_back()),
                ("approval_pending", waiting),
            ] {
                set(
                    "rgnix_rollout_state",
                    &[owner, name],
                    u8::from(enabled) as f64,
                );
            }
        }
        set(
            "rgnix_metrics_omitted",
            &["rollouts"],
            rollouts.len().saturating_sub(LIMIT) as f64,
        );
        let controls = shared.controls.active.load();
        let results = shared
            .metric_results
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = k8s_openapi::chrono::Utc::now().timestamp() as f64;
        for name in controls.metrics.gates.keys() {
            let result = results.get(name).filter(|r| r.digest == controls.digest);
            let passed = result.is_some_and(|r| r.passed(&controls.digest));
            set("rgnix_metric_gate_passed", &[name], u8::from(passed) as f64);
            set(
                "rgnix_metric_gate_checked_timestamp_seconds",
                &[name],
                result.map_or(0.0, |r| now - r.checked.elapsed().as_secs_f64()),
            );
        }
        drop(results);
        if !shared.file_mode {
            set(
                "rgnix_ingress_leader",
                &[],
                u8::from(
                    shared
                        .telemetry
                        .controller
                        .leader_until
                        .load(Ordering::Relaxed) as f64
                        > now,
                ) as f64,
            );
        }
        set(
            "rgnix_metric_labels",
            &[],
            shared
                .telemetry
                .labels
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len() as f64,
        );
        set("rgnix_metric_label_limit", &[], LIMIT as f64);
        metrics.values().flat_map(Collector::collect).collect()
    }
}
