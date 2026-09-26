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

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct Samples {
    pub count: u64,
    pub errors: u64,
    pub slow: u64,
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
    pub(crate) fn sample_key(&self) -> String {
        self.sample_key_at(&self.progress())
    }
    fn sample_key_at(&self, progress: &Progress) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    &self.kind,
                    &self.owner,
                    &self.uid,
                    self.policy_hash(),
                    progress.stage,
                    progress.started_at
                ))
                .unwrap()
            )
        )
    }
    pub(crate) fn sample_report(&self) -> (String, Samples) {
        let progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        let key = self.sample_key_at(&progress);
        let mut result = Samples::default();
        let Some(rule) = &self.policy.rollback else {
            return (key, result);
        };
        let window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        for (_, failed, elapsed) in window
            .samples
            .iter()
            .filter(|s| s.0.elapsed().as_secs() <= rule.window_seconds)
        {
            result.count += 1;
            result.errors += u64::from(*failed);
            result.slow += u64::from(rule.max_p95_ms.is_some_and(|limit| *elapsed > limit));
        }
        (key, result)
    }
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
    pub(crate) fn next_progress(
        &self,
        now: i64,
        gates_pass: bool,
        samples: Option<Samples>,
    ) -> Option<Progress> {
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
        if self.rolled_back() {
            return None;
        }
        let samples = samples?;
        if samples.count < step.min_requests as u64 {
            return None;
        }
        if samples.count > 0
            && let Some(rule) = &self.policy.rollback
        {
            if samples.errors * 100 >= samples.count * u64::from(rule.error_percent) {
                return None;
            }
            if rule.max_p95_ms.is_some() && samples.slow > samples.count / 20 {
                return None;
            }
        }
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
