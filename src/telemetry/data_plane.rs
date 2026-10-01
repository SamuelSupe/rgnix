use anyhow::Result;
use prometheus::{GaugeVec, IntGaugeVec, Registry};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub struct DataPlane {
    enabled: AtomicBool,
    timeout_ms: AtomicU64,
    origin: Instant,
    workers: Mutex<Vec<Arc<Worker>>>,
    healthy: IntGaugeVec,
    age: GaugeVec,
}

pub(crate) struct Worker {
    name: String,
    origin: Instant,
    heartbeat: AtomicU64,
}

impl DataPlane {
    pub fn new(registry: &Registry) -> Result<Arc<Self>> {
        let healthy = prometheus::register_int_gauge_vec_with_registry!(
            "rgnix_runtime_service_healthy",
            "Native runtime service reactor heartbeat is fresh",
            &["service"],
            registry
        )?;
        let age = prometheus::register_gauge_vec_with_registry!(
            "rgnix_runtime_service_heartbeat_age_seconds",
            "Age of the native runtime service reactor heartbeat",
            &["service"],
            registry
        )?;
        Ok(Arc::new(Self {
            enabled: AtomicBool::new(false),
            timeout_ms: AtomicU64::new(5000),
            origin: Instant::now(),
            workers: Mutex::new(Vec::new()),
            healthy,
            age,
        }))
    }
    pub(crate) fn enable(&self, timeout: Duration) {
        self.timeout_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
        self.enabled.store(true, Ordering::Release);
    }
    pub(crate) fn register(&self, name: String) -> Arc<Worker> {
        let worker = Arc::new(Worker {
            name,
            origin: self.origin,
            heartbeat: AtomicU64::new(0),
        });
        self.workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(worker.clone());
        worker
    }
    pub fn healthy(&self) -> bool {
        if !self.enabled.load(Ordering::Acquire) {
            return true;
        }
        let now = self.origin.elapsed().as_millis() as u64 + 1;
        let timeout = self.timeout_ms.load(Ordering::Relaxed);
        let workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        !workers.is_empty()
            && workers.iter().all(|w| {
                let last = w.heartbeat.load(Ordering::Acquire);
                last != 0 && now.saturating_sub(last) <= timeout
            })
    }
    pub fn update_metrics(&self) {
        let now = self.origin.elapsed().as_millis() as u64 + 1;
        let timeout = self.timeout_ms.load(Ordering::Relaxed);
        for worker in self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let last = worker.heartbeat.load(Ordering::Acquire);
            let age = now.saturating_sub(last);
            self.age
                .with_label_values(&[&worker.name])
                .set(age as f64 / 1000.);
            self.healthy
                .with_label_values(&[&worker.name])
                .set(i64::from(last != 0 && age <= timeout));
        }
    }
}

impl Worker {
    pub(crate) async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            self.heartbeat.store(
                self.origin.elapsed().as_millis() as u64 + 1,
                Ordering::Release,
            );
        }
    }
}
