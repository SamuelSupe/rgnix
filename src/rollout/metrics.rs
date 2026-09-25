use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path};

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
    pub async fn evaluate(&self, name: &str, client: &reqwest::Client) -> bool {
        let Some(gate) = self.gates.get(name) else {
            return false;
        };
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
            Ok::<_, anyhow::Error>(
                value.is_finite()
                    && gate.min.is_none_or(|min| value >= min)
                    && gate.max.is_none_or(|max| value <= max),
            )
        }
        .await;
        result.unwrap_or(false)
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
        let result = results.get(name).filter(|(digest, _, _)| digest == &controls.digest);
        let age = result.map(|(_, _, at)| at.elapsed().as_secs());
        let passed = result.is_some_and(|(_, pass, _)| *pass) && age.is_some_and(|age| age <= 60);
        (name, serde_json::json!({"passed":passed,"checked_seconds_ago":age,"fresh":age.is_some_and(|age|age<=60)}))
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
                        (state.digest.clone(), passed, std::time::Instant::now()),
                    );
            }
            self.0
                .metric_results
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|name, _| state.metrics.gates.contains_key(name));
        }
    }
}
