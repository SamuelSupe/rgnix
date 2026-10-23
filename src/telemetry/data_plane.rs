use anyhow::Result;
use prometheus::{GaugeVec, Histogram, HistogramVec, IntGaugeVec, Registry};
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
    wakeup_probe: AtomicBool,
    wakeup_lag: HistogramVec,
}

pub(crate) struct Worker {
    name: String,
    origin: Instant,
    heartbeat: AtomicU64,
    wakeup_lag: Option<Histogram>,
}

const PROBE_INTERVAL: Duration = Duration::from_millis(1);

pub(crate) struct WakeupProbe {
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WakeupProbe {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
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
        let wakeup_lag = prometheus::register_histogram_vec_with_registry!(
            "rgnix_runtime_wakeup_lag_seconds",
            "Diagnostic wakeup delay after a scheduled deadline; reference uses an independent thread",
            &["service"],
            vec![
                0.00025, 0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.25, 1.0
            ],
            registry
        )?;
        Ok(Arc::new(Self {
            enabled: AtomicBool::new(false),
            timeout_ms: AtomicU64::new(5000),
            origin: Instant::now(),
            workers: Mutex::new(Vec::new()),
            healthy,
            age,
            wakeup_probe: AtomicBool::new(false),
            wakeup_lag,
        }))
    }
    pub(crate) fn enable(&self, timeout: Duration, probe: bool) -> Result<Option<WakeupProbe>> {
        self.timeout_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
        self.enabled.store(true, Ordering::Release);
        self.wakeup_probe.store(probe, Ordering::Relaxed);
        if !probe {
            return Ok(None);
        }
        let metric = self.wakeup_lag.with_label_values(&["reference"]);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let thread = std::thread::Builder::new()
            .name("rgnix-wakeup-reference".into())
            .spawn(move || {
                let mut deadline = Instant::now() + PROBE_INTERVAL;
                while !stop.load(Ordering::Acquire) {
                    std::thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let now = Instant::now();
                    if now < deadline {
                        continue;
                    }
                    metric.observe(now.saturating_duration_since(deadline).as_secs_f64());
                    deadline = now + PROBE_INTERVAL;
                }
            })?;
        Ok(Some(WakeupProbe {
            stopped,
            thread: Some(thread),
        }))
    }
    pub(crate) fn register(&self, name: String) -> Arc<Worker> {
        let wakeup_lag = self
            .wakeup_probe
            .load(Ordering::Relaxed)
            .then(|| self.wakeup_lag.with_label_values(&[&name]));
        let worker = Arc::new(Worker {
            name,
            origin: self.origin,
            heartbeat: AtomicU64::new(0),
            wakeup_lag,
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
        let period = if self.wakeup_lag.is_some() {
            PROBE_INTERVAL
        } else {
            Duration::from_millis(250)
        };
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let deadline = interval.tick().await;
            let now = Instant::now();
            self.heartbeat.store(
                now.saturating_duration_since(self.origin).as_millis() as u64 + 1,
                Ordering::Release,
            );
            if let Some(metric) = &self.wakeup_lag {
                metric.observe(
                    now.saturating_duration_since(deadline.into_std())
                        .as_secs_f64(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reactor_stall_is_observed_while_reference_keeps_running_and_stops_on_drop() {
        let data = DataPlane::new(&Registry::new()).unwrap();
        let reference = data.enable(Duration::from_secs(5), true).unwrap().unwrap();
        let worker = data.register("blocked-test".into());
        let reactor = worker.wakeup_lag.clone().unwrap();
        let independent = data.wakeup_lag.with_label_values(&["reference"]);
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let task = tokio::spawn(worker.run());
                tokio::time::sleep(Duration::from_millis(5)).await;
                let before = independent.get_sample_count();
                let blocked = Instant::now();
                std::thread::sleep(Duration::from_millis(30));
                while independent.get_sample_count() == before
                    && blocked.elapsed() < Duration::from_secs(1)
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(independent.get_sample_count() > before);
                tokio::time::sleep(Duration::from_millis(3)).await;
                assert!(reactor.get_sample_sum() >= 0.02);
                task.abort();
            });
        drop(reference);
        let count = independent.get_sample_count();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(independent.get_sample_count(), count);
    }
}
