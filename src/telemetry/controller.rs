use anyhow::Result;
use prometheus::{IntCounterVec, IntGauge, IntGaugeVec, Registry};
use std::sync::atomic::AtomicI64;

pub struct Controller {
    pub watch_events: IntCounterVec,
    pub watch_errors: IntCounterVec,
    pub watch_last_event: IntGaugeVec,
    pub resources: IntGaugeVec,
    pub selected: IntGauge,
    pub lease_results: IntCounterVec,
    pub leader_until: AtomicI64,
    streams: IntGaugeVec,
}

impl Controller {
    pub fn new(registry: &Registry) -> Result<Self> {
        use prometheus::*;
        Ok(Self {
            watch_events: register_int_counter_vec_with_registry!(
                "rgnix_ingress_watch_events_total",
                "Successful Kubernetes watch events",
                &["kind", "event"],
                registry
            )?,
            watch_errors: register_int_counter_vec_with_registry!(
                "rgnix_ingress_watch_errors_total",
                "Kubernetes watch errors",
                &["kind"],
                registry
            )?,
            watch_last_event: register_int_gauge_vec_with_registry!(
                "rgnix_ingress_watch_last_event_timestamp_seconds",
                "Most recent successful watch event, not a liveness heartbeat",
                &["kind"],
                registry
            )?,
            resources: register_int_gauge_vec_with_registry!(
                "rgnix_ingress_cached_resources",
                "Resources in watch caches before dependency filtering",
                &["kind"],
                registry
            )?,
            selected: register_int_gauge_with_registry!(
                "rgnix_ingress_selected_resources",
                "Ingress resources selected by the active class",
                registry
            )?,
            lease_results: register_int_counter_vec_with_registry!(
                "rgnix_ingress_lease_attempts_total",
                "Lease acquisition and renewal attempts",
                &["result"],
                registry
            )?,
            leader_until: AtomicI64::new(0),
            streams: register_int_gauge_vec_with_registry!(
                "rgnix_ingress_watch_streams",
                "Watch streams grouped by kind and state",
                &["kind", "state"],
                registry
            )?,
        })
    }
    pub fn watch(&self, kind: &str) -> Watch {
        let total = self.streams.with_label_values(&[kind, "configured"]);
        total.inc();
        Watch {
            total,
            synced: self.streams.with_label_values(&[kind, "synchronized"]),
            healthy: self.streams.with_label_values(&[kind, "healthy"]),
            is_synced: false,
            is_healthy: false,
        }
    }
}

pub struct Watch {
    total: IntGauge,
    synced: IntGauge,
    healthy: IntGauge,
    is_synced: bool,
    is_healthy: bool,
}
impl Watch {
    pub fn synchronized(&mut self, value: bool) {
        self.synced
            .add(i64::from(value) - i64::from(self.is_synced));
        self.is_synced = value;
    }
    pub fn healthy(&mut self, value: bool) {
        self.healthy
            .add(i64::from(value) - i64::from(self.is_healthy));
        self.is_healthy = value;
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.synchronized(false);
        self.healthy(false);
        self.total.dec();
    }
}
