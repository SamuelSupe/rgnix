use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, time::Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollback {
    pub consecutive_failures: u32,
    pub failure_seconds: u64,
    #[serde(default)]
    pub unavailable: Unavailable,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    #[default]
    Pause,
    Rollback,
}
impl Rollback {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=120).contains(&self.consecutive_failures) && self.failure_seconds <= 3600,
            "metric rollback requires 1..120 failures and at most 3600 failure seconds"
        );
        Ok(())
    }
}
pub struct Observation {
    pub digest: String,
    pub passed: Option<bool>,
    pub checked: Instant,
}
impl Observation {
    pub fn fresh(&self, digest: &str) -> bool {
        self.digest == digest && self.checked.elapsed().as_secs() <= 60
    }
    pub fn passed(&self, digest: &str) -> bool {
        self.fresh(digest) && self.passed == Some(true)
    }
}
#[derive(Default)]
pub(super) struct Failures {
    digest: String,
    stage: usize,
    gates: BTreeMap<String, (Instant, Instant, u32)>,
}
impl super::State {
    pub fn evaluate_metrics(&self, digest: &str, results: &BTreeMap<String, Observation>) -> bool {
        let Some(rule) = &self.policy.metric_rollback else {
            return false;
        };
        let stage = self.stage();
        let mut failures = self
            .metric_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if failures.digest != digest || failures.stage != stage {
            *failures = Failures {
                digest: digest.into(),
                stage,
                ..Default::default()
            };
        }
        for name in &self.policy.metric_gates {
            let Some(result) = results.get(name).filter(|r| r.fresh(digest)) else {
                failures.gates.remove(name);
                continue;
            };
            let failing = result.passed == Some(false)
                || (result.passed.is_none() && matches!(rule.unavailable, Unavailable::Rollback));
            if !failing {
                failures.gates.remove(name);
                continue;
            }
            let entry =
                failures
                    .gates
                    .entry(name.clone())
                    .or_insert((result.checked, result.checked, 1));
            if result.checked > entry.1 {
                // A gap in provider observations cannot count as continuous failure.
                if result.checked.duration_since(entry.1).as_secs() > 60 {
                    *entry = (result.checked, result.checked, 1);
                } else {
                    entry.1 = result.checked;
                    entry.2 = entry.2.saturating_add(1);
                }
            }
            if entry.2 >= rule.consecutive_failures
                && entry.1.duration_since(entry.0).as_secs() >= rule.failure_seconds
            {
                let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
                if !window.rolled_back {
                    window.rolled_back = true;
                    window.reason = format!(
                        "external metric {name} {}",
                        if result.passed.is_none() {
                            "unavailable"
                        } else {
                            "threshold"
                        }
                    );
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};

    fn state(unavailable: &str) -> Arc<crate::rollout::State> {
        let policy = crate::rollout::Policy::parse(&serde_json::json!({
            "revision":"business-1", "backends":[{"service":"stable:80","weight":10},{"service":"canary:80","weight":90}],
            "rollback":{"fallback":"stable:80","min_requests":100,"error_percent":50,"window_seconds":60},
            "metric_gates":["orders"],
            "metric_rollback":{"consecutive_failures":2,"failure_seconds":2,"unavailable":unavailable}
        }).to_string()).unwrap();
        crate::rollout::Rollouts::default().get(
            "ns/app",
            "uid",
            policy,
            BTreeMap::from([
                ("stable:80".into(), "stable".into()),
                ("canary:80".into(), "canary".into()),
            ]),
        )
    }
    fn observe(
        state: &crate::rollout::State,
        digest: &str,
        passed: Option<bool>,
        checked: Instant,
    ) -> bool {
        state.evaluate_metrics(
            "current",
            &BTreeMap::from([(
                "orders".into(),
                Observation {
                    digest: digest.into(),
                    passed,
                    checked,
                },
            )]),
        )
    }
    #[test]
    fn sustained_business_failure_resets_on_recovery_and_rolls_back_http_success() {
        let state = state("pause");
        let start = Instant::now() - Duration::from_secs(10);
        state.completed("canary", false, Duration::from_millis(1), 0);
        assert!(!observe(&state, "current", Some(false), start));
        assert!(!observe(&state, "current", Some(false), start));
        assert!(!observe(
            &state,
            "current",
            Some(true),
            start + Duration::from_secs(2)
        ));
        assert!(!observe(
            &state,
            "current",
            Some(false),
            start + Duration::from_secs(4)
        ));
        assert!(observe(
            &state,
            "current",
            Some(false),
            start + Duration::from_secs(6)
        ));
        assert_eq!(state.select(&Default::default()), "stable");
        assert!(!observe(
            &state,
            "current",
            Some(false),
            start + Duration::from_secs(8)
        ));
    }
    #[test]
    fn unavailable_provider_requires_explicit_rollback_policy() {
        let start = Instant::now() - Duration::from_secs(4);
        for mode in ["pause", "rollback"] {
            let state = state(mode);
            assert!(!observe(&state, "current", None, start));
            assert_eq!(
                observe(&state, "current", None, start + Duration::from_secs(3)),
                mode == "rollback"
            );
        }
    }
    #[test]
    fn stale_or_reconfigured_observations_cannot_trigger_rollback() {
        let state = state("rollback");
        let now = Instant::now();
        assert!(!observe(
            &state,
            "current",
            Some(false),
            now - Duration::from_secs(90)
        ));
        assert!(!observe(
            &state,
            "current",
            Some(false),
            now - Duration::from_secs(5)
        ));
        assert!(!observe(
            &state,
            "old-provider",
            Some(false),
            now - Duration::from_secs(3)
        ));
        assert!(!observe(&state, "current", Some(false), now));
        assert!(!state.rolled_back());
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub url: String,
    pub pointer: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub bearer_token: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metrics {
    pub gates: BTreeMap<String, Gate>,
}
impl Metrics {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let metrics: Self =
            serde_json::from_slice(&crate::controls::read_bounded(path, 1024 * 1024)?)?;
        ensure!(
            metrics.gates.len() <= 128,
            "at most 128 external metric gates"
        );
        for (name, gate) in &metrics.gates {
            ensure!(
                !name.is_empty() && name.len() <= 128,
                "invalid metric gate name"
            );
            let url = url::Url::parse(&gate.url)?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none(),
                "metric gate requires HTTP(S) without URL credentials"
            );
            ensure!(
                gate.pointer.starts_with('/') && gate.pointer.len() <= 512,
                "metric gate requires a JSON pointer"
            );
            ensure!(
                gate.min.is_some() || gate.max.is_some(),
                "metric gate requires min or max"
            );
            ensure!(
                gate.min
                    .iter()
                    .chain(gate.max.iter())
                    .all(|v| v.is_finite())
                    && gate.min.zip(gate.max).is_none_or(|(min, max)| min <= max),
                "invalid metric threshold"
            );
            ensure!(
                gate.bearer_token
                    .as_ref()
                    .is_none_or(|t| t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_graphic())),
                "invalid metric bearer token"
            );
        }
        Ok(metrics)
    }
    pub async fn evaluate(&self, name: &str, client: &reqwest::Client) -> Option<bool> {
        let gate = self.gates.get(name)?;
        let result = async {
            let mut request = client
                .get(&gate.url)
                .timeout(std::time::Duration::from_secs(2));
            if let Some(token) = &gate.bearer_token {
                request = request.bearer_auth(token);
            }
            let mut response = request.send().await?.error_for_status()?;
            ensure!(
                response.status().is_success(),
                "metric endpoint did not return success"
            );
            let mut bytes = vec![];
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    bytes.len() + chunk.len() <= 65536,
                    "metric response exceeds 64 KiB"
                );
                bytes.extend_from_slice(&chunk);
            }
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            let value = value
                .pointer(&gate.pointer)
                .and_then(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .ok_or_else(|| anyhow::anyhow!("metric value missing"))?;
            ensure!(value.is_finite(), "metric value must be finite");
            Ok::<_, anyhow::Error>(
                gate.min.is_none_or(|min| value >= min) && gate.max.is_none_or(|max| value <= max),
            )
        }
        .await;
        result.ok()
    }
}

pub struct Poller(pub std::sync::Arc<crate::runtime::Shared>);

pub fn diagnostic<'a>(
    shared: &crate::runtime::Shared,
    names: impl IntoIterator<Item = &'a str>,
) -> serde_json::Value {
    let controls = shared.controls.active.load();
    let results = shared
        .metric_results
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    serde_json::json!(names.into_iter().map(|name| {
        let result = results.get(name).filter(|r| r.digest == controls.digest);
        let age = result.map(|r| r.checked.elapsed().as_secs());
        let passed = result.is_some_and(|r| r.passed(&controls.digest));
        (name, serde_json::json!({"passed":passed,"available":result.is_some_and(|r|r.passed.is_some()),"checked_seconds_ago":age,"fresh":age.is_some_and(|age|age<=60)}))
    }).collect::<BTreeMap<_, _>>())
}
#[async_trait::async_trait]
impl pingora::services::background::BackgroundService for Poller {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        use futures::{StreamExt, stream};
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tokio::select! { _=shutdown.changed()=>break, _=tick.tick()=>{} }
            let state = self.0.controls.active.load_full();
            let names: Vec<_> = state.metrics.gates.keys().cloned().collect();
            let mut checks = stream::iter(names.into_iter().map(|name| {
                let state = state.clone();
                let client = self.0.auth_client.clone();
                async move {
                    let passed = state.metrics.evaluate(&name, &client).await;
                    (name, passed)
                }
            }))
            .buffer_unordered(8);
            loop {
                let result =
                    tokio::select! { _=shutdown.changed()=>return, result=checks.next()=>result };
                let Some((name, passed)) = result else {
                    break;
                };
                self.0
                    .metric_results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        name,
                        Observation {
                            digest: state.digest.clone(),
                            passed,
                            checked: Instant::now(),
                        },
                    );
            }
            self.0
                .metric_results
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|name, _| state.metrics.gates.contains_key(name));
            let results = self
                .0
                .metric_results
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for rollout in self.0.rollouts.active() {
                if rollout.evaluate_metrics(&state.digest, &results) {
                    self.0.telemetry.rollbacks.inc();
                    log::warn!(
                        "external metric triggered rollback for {} revision {}",
                        rollout.owner,
                        rollout.policy.revision
                    );
                }
            }
        }
    }
}
