use super::*;
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub weights: BTreeMap<String, u32>,
    pub duration_seconds: u64,
    #[serde(default = "minimum")]
    pub min_requests: usize,
    #[serde(default)]
    pub approval: bool,
}
fn minimum() -> usize {
    20
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub revision: String,
    pub policy_hash: String,
    pub stage: usize,
    pub started_at: i64,
    pub paused: bool,
    pub approved_stage: Option<usize>,
    pub promoted: bool,
}
impl State {
    pub fn policy_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&self.policy).unwrap())
        )
    }
    pub fn progress(&self) -> Progress {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn stage(&self) -> usize {
        self.progress().stage
    }
    pub fn sync_progress(&self, value: Option<&str>) -> Result<()> {
        let progress = if let Some(value) = value {
            let progress: Progress = serde_json::from_str(value)?;
            if progress.revision != self.policy.revision
                || progress.policy_hash != self.policy_hash()
            {
                return Ok(());
            }
            ensure!(
                progress.stage < self.policy.steps.len().max(1)
                    && progress.started_at >= 0
                    && progress
                        .approved_stage
                        .is_none_or(|stage| stage < self.policy.steps.len().max(1)),
                "invalid rollout progress"
            );
            progress
        } else {
            Progress::default()
        };
        let mut current = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        let changed = current.stage != progress.stage || current.started_at != progress.started_at;
        *current = progress;
        drop(current);
        if changed {
            self.window
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .samples
                .clear();
        }
        Ok(())
    }
    pub fn stage_weights(&self) -> Vec<Target> {
        let stage = self.stage();
        let weights = self.policy.steps.get(stage).map(|step| &step.weights);
        self.policy
            .backends
            .iter()
            .map(|target| Target {
                service: target.service.clone(),
                weight: weights.map_or(target.weight, |w| w[&target.service]),
            })
            .collect()
    }
    pub fn next_progress(&self, now: i64, gates_pass: bool) -> Option<Progress> {
        if self.policy.steps.is_empty() {
            return None;
        }
        let mut progress = self.progress();
        if progress.started_at == 0 {
            return Some(Progress {
                revision: self.policy.revision.clone(),
                policy_hash: self.policy_hash(),
                started_at: now,
                ..Default::default()
            });
        }
        let step = &self.policy.steps[progress.stage];
        if progress.paused
            || progress.promoted
            || !gates_pass
            || now - progress.started_at < step.duration_seconds as i64
            || (step.approval && progress.approved_stage != Some(progress.stage))
        {
            return None;
        }
        let window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if window.rolled_back {
            return None;
        }
        let recent: Vec<_> = window
            .samples
            .iter()
            .filter(|s| {
                s.0.elapsed().as_secs()
                    <= self
                        .policy
                        .rollback
                        .as_ref()
                        .map_or(60, |r| r.window_seconds)
            })
            .collect();
        if recent.len() < step.min_requests {
            return None;
        }
        if !recent.is_empty()
            && let Some(rule) = &self.policy.rollback
        {
            if recent.iter().filter(|s| s.1).count() * 100
                >= recent.len() * rule.error_percent as usize
            {
                return None;
            }
            if let Some(limit) = rule.max_p95_ms {
                let mut times: Vec<_> = recent.iter().map(|s| s.2).collect();
                times.sort_unstable();
                if times[(times.len() * 95).div_ceil(100).saturating_sub(1)] > limit {
                    return None;
                }
            }
        }
        drop(window);
        if progress.stage + 1 == self.policy.steps.len() {
            progress.promoted = true;
        } else {
            progress.stage += 1;
            progress.started_at = now;
            progress.approved_stage = None;
        }
        Some(progress)
    }
    pub fn progress_diagnostic(&self) -> Value {
        serde_json::json!({"progress":self.progress(),"weights":self.stage_weights(),"metric_gates":self.policy.metric_gates})
    }
}
