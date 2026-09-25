pub mod metrics;
mod progress;
use anyhow::{Result, ensure};
pub use progress::{Progress, Step};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub service: String,
    pub weight: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mirror {
    pub service: String,
    pub percent: u32,
    pub max_body_bytes: usize,
    pub timeout_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollback {
    pub fallback: String,
    pub min_requests: usize,
    pub error_percent: u32,
    pub window_seconds: u64,
    pub max_p95_ms: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub revision: String,
    pub backends: Vec<Target>,
    pub mirror: Option<Mirror>,
    pub rollback: Option<Rollback>,
    #[serde(default)]
    pub steps: Vec<Step>,
    pub cohort: Option<String>,
    #[serde(default)]
    pub metric_gates: Vec<String>,
}
impl Policy {
    pub fn parse(value: &str) -> Result<Self> {
        let policy: Self = serde_json::from_str(value)?;
        ensure!(
            !policy.revision.is_empty() && policy.revision.len() <= 64,
            "traffic revision requires 1..64 bytes"
        );
        ensure!(
            (1..=8).contains(&policy.backends.len())
                && policy.backends.iter().all(|b| b.weight <= 10000)
                && policy.backends.iter().map(|b| b.weight).sum::<u32>() > 0,
            "traffic requires 1..8 backends and positive total weight"
        );
        let mut unique = std::collections::BTreeSet::new();
        for backend in &policy.backends {
            ensure!(unique.insert(&backend.service), "duplicate traffic backend");
        }
        if let Some(m) = &policy.mirror {
            ensure!(
                m.percent <= 100
                    && m.max_body_bytes <= 1024 * 1024
                    && (1..=10000).contains(&m.timeout_ms),
                "invalid mirror percentage/body/timeout"
            );
        }
        if let Some(r) = &policy.rollback {
            ensure!(
                unique.contains(&r.fallback)
                    && (1..=10000).contains(&r.min_requests)
                    && (1..=100).contains(&r.error_percent)
                    && (1..=3600).contains(&r.window_seconds)
                    && r.max_p95_ms.is_none_or(|v| (1..=3600000).contains(&v)),
                "invalid rollback thresholds or fallback"
            );
        }
        if let Some(key) = &policy.cohort {
            let key = crate::traffic::Key::parse(key)?;
            ensure!(
                !matches!(key, crate::traffic::Key::Route),
                "cohort requires a client-specific key"
            );
        }
        ensure!(
            policy.steps.len() <= 16 && policy.metric_gates.len() <= 8,
            "at most 16 rollout stages and 8 metric gates"
        );
        ensure!(
            policy.metric_gates.is_empty() || !policy.steps.is_empty(),
            "metric gates require rollout steps"
        );
        for step in &policy.steps {
            ensure!(
                policy.rollback.is_some(),
                "staged rollouts require rollback thresholds and fallback"
            );
            ensure!(
                step.duration_seconds <= 86400 && step.min_requests <= 10000,
                "invalid stage duration or sample budget"
            );
            ensure!(
                step.weights.len() == unique.len()
                    && step
                        .weights
                        .iter()
                        .all(|(k, v)| unique.contains(k) && *v <= 10000)
                    && step.weights.values().sum::<u32>() > 0,
                "stage weights must cover every declared target with positive total weight"
            );
        }
        Ok(policy)
    }
    pub fn services(&self) -> impl Iterator<Item = &str> {
        self.backends
            .iter()
            .map(|b| b.service.as_str())
            .chain(self.mirror.iter().map(|m| m.service.as_str()))
    }
}
#[derive(Default)]
pub struct Rollouts {
    states: Mutex<BTreeMap<String, Arc<State>>>,
}
pub struct State {
    pub policy: Policy,
    pub owner: String,
    pub uid: String,
    pub backends: BTreeMap<String, String>,
    cursor: AtomicU64,
    window: Mutex<Window>,
    progress: Mutex<Progress>,
}
#[derive(Default)]
struct Window {
    samples: VecDeque<(Instant, bool, u64)>,
    rolled_back: bool,
    reason: String,
}
impl Rollouts {
    pub fn active(&self) -> Vec<Arc<State>> {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }
    pub fn pending(&self) -> Vec<(String, String, String)> {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|s| {
                s.window
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rolled_back
            })
            .map(|s| (s.owner.clone(), s.uid.clone(), s.policy.revision.clone()))
            .collect()
    }
    pub fn get(
        &self,
        owner: &str,
        uid: &str,
        policy: Policy,
        backends: BTreeMap<String, String>,
    ) -> Arc<State> {
        use sha2::{Digest, Sha256};
        let key = format!(
            "{owner}:{uid}:{:x}",
            Sha256::digest(serde_json::to_vec(&policy).unwrap())
        );
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        states.retain(|k, v| k == &key || Arc::strong_count(v) > 1);
        states
            .entry(key)
            .or_insert_with(|| {
                Arc::new(State {
                    owner: owner.into(),
                    uid: uid.into(),
                    policy,
                    backends,
                    cursor: AtomicU64::new(0),
                    window: Mutex::new(Window::default()),
                    progress: Mutex::new(Progress::default()),
                })
            })
            .clone()
    }
}
impl State {
    pub fn enforce(&self, backend: String) -> String {
        let window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if window.rolled_back {
            return self.backends[&self.policy.rollback.as_ref().unwrap().fallback].clone();
        }
        backend
    }
    pub fn force_rollback(&self) {
        if self.policy.rollback.is_some() {
            let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
            window.rolled_back = true;
            window.reason = "persisted rollback".into();
        }
    }
    pub fn select(&self, request: &crate::script::RequestData) -> String {
        self.select_at(request, self.cursor.fetch_add(1, Ordering::Relaxed))
    }
    pub fn select_at(&self, request: &crate::script::RequestData, cursor: u64) -> String {
        if self
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rolled_back
        {
            return self.backends[&self.policy.rollback.as_ref().unwrap().fallback].clone();
        }
        let weights = self.stage_weights();
        let total = weights.iter().map(|b| u64::from(b.weight)).sum::<u64>();
        let mut n = if let Some(key) = &self.policy.cohort {
            use sha2::{Digest, Sha256};
            let value = crate::traffic::Key::parse(key)
                .unwrap()
                .value(request, &request.claims);
            let value = if value.is_empty() {
                &request.remote_addr
            } else {
                &value
            };
            let mut digest = Sha256::new();
            digest.update(self.policy.revision.as_bytes());
            digest.update([0]);
            digest.update(value.as_bytes());
            let hash: [u8; 32] = digest.finalize().into();
            u64::from_be_bytes(hash[..8].try_into().unwrap()) % 10000 * total / 10000
        } else {
            cursor % total
        };
        for target in &weights {
            if n < u64::from(target.weight) {
                return self.backends[&target.service].clone();
            }
            n -= u64::from(target.weight);
        }
        unreachable!()
    }
    pub fn completed(&self, backend: &str, failed: bool, elapsed: Duration, stage: usize) -> bool {
        if stage != self.stage() {
            return false;
        }
        let Some(rule) = &self.policy.rollback else {
            return false;
        };
        if self.backends[&rule.fallback] == backend
            || !self
                .policy
                .backends
                .iter()
                .any(|target| self.backends[&target.service] == backend)
        {
            return false;
        }
        let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if window.rolled_back {
            return false;
        }
        let now = Instant::now();
        window.samples.push_back((
            now,
            failed,
            elapsed.as_millis().min(u64::MAX as u128) as u64,
        ));
        while window
            .samples
            .front()
            .is_some_and(|s| now.duration_since(s.0).as_secs() > rule.window_seconds)
            || window.samples.len() > 10000
        {
            window.samples.pop_front();
        }
        if window.samples.len() < rule.min_requests {
            return false;
        }
        let errors = window.samples.iter().filter(|s| s.1).count();
        let error_limit = errors * 100 >= window.samples.len() * rule.error_percent as usize;
        let latency_limit = rule.max_p95_ms.is_some_and(|limit| {
            let mut times: Vec<_> = window.samples.iter().map(|s| s.2).collect();
            times.sort_unstable();
            times[(times.len() * 95).div_ceil(100).saturating_sub(1)] > limit
        });
        if error_limit || latency_limit {
            window.rolled_back = true;
            window.reason = if error_limit {
                "error threshold"
            } else {
                "p95 threshold"
            }
            .into();
            return true;
        }
        false
    }
    pub fn diagnostic(&self) -> serde_json::Value {
        let w = self.window.lock().unwrap_or_else(|e| e.into_inner());
        let mut result = serde_json::json!({"policy":self.policy,"rolled_back":w.rolled_back,"reason":w.reason,"samples":w.samples.len()});
        drop(w);
        result["release"] = self.progress_diagnostic();
        result
    }
}
