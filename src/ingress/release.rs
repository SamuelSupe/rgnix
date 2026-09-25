use super::*;
use crate::rollout::Progress;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const PROGRESS: &str = "rgnix.io/rollout-state";
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Pause,
    Resume,
    Approve,
    Rollback,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub owner: String,
    pub revision: String,
    pub operation: Operation,
    pub stage: Option<usize>,
}
fn initial(state: &crate::rollout::State) -> Progress {
    Progress {
        revision: state.policy.revision.clone(),
        policy_hash: state.policy_hash(),
        started_at: k8s_openapi::chrono::Utc::now().timestamp(),
        ..Default::default()
    }
}
pub async fn command(shared: &Shared, command: Command) -> Result<()> {
    let state = shared
        .rollouts
        .active()
        .into_iter()
        .find(|s| s.owner == command.owner && s.policy.revision == command.revision)
        .context("active rollout revision not found")?;
    let (namespace, name) = command
        .owner
        .split_once('/')
        .context("owner must be namespace/name")?;
    let api: Api<Ingress> = Api::namespaced(Client::try_default().await?, namespace);
    let current = tokio::time::timeout(Duration::from_secs(5), api.get(name)).await??;
    ensure!(
        current.uid().as_deref() == Some(&state.uid),
        "Ingress owner changed"
    );
    let policy = current
        .annotations()
        .get("rgnix.io/traffic-policy")
        .context("traffic policy removed")?;
    ensure!(
        serde_json::to_value(crate::rollout::Policy::parse(policy)?)?
            == serde_json::to_value(&state.policy)?,
        "traffic policy changed; refresh diagnostics"
    );
    let mut progress = current
        .annotations()
        .get(PROGRESS)
        .map(|v| serde_json::from_str::<Progress>(v))
        .transpose()?
        .filter(|p| p.policy_hash == state.policy_hash())
        .unwrap_or_else(|| initial(&state));
    let (key, value) = match command.operation {
        Operation::Rollback => {
            ensure!(
                state.policy.rollback.is_some(),
                "rollback fallback is not configured"
            );
            (
                "rgnix.io/rolled-back-revision",
                state.policy.revision.clone(),
            )
        }
        operation => {
            ensure!(
                !state.policy.steps.is_empty() && !progress.promoted,
                "rollout has no active stages"
            );
            match operation {
                Operation::Pause => progress.paused = true,
                Operation::Resume => {
                    progress.paused = false;
                    progress.started_at = k8s_openapi::chrono::Utc::now().timestamp();
                }
                Operation::Approve => {
                    ensure!(
                        command.stage == Some(progress.stage),
                        "approval requires the current stage number"
                    );
                    progress.approved_stage = Some(progress.stage);
                }
                Operation::Rollback => unreachable!(),
            }
            (PROGRESS, serde_json::to_string(&progress)?)
        }
    };
    let patch = serde_json::json!({"metadata":{"resourceVersion":current.resource_version(),"annotations":{key:value}}});
    tokio::time::timeout(
        Duration::from_secs(5),
        api.patch(name, &Default::default(), &kube::api::Patch::Merge(&patch)),
    )
    .await??;
    Ok(())
}

pub(super) async fn reconcile(
    client: &Client,
    shared: &Shared,
    options: &Options,
    resources: &Resources,
) {
    let states: Vec<_> = shared
        .rollouts
        .active()
        .into_iter()
        .filter(|s| !s.policy.steps.is_empty())
        .collect();
    if states.is_empty() {
        return;
    }
    let reporter =
        super::status::Reporter::new(client.clone(), options.clone(), shared.telemetry.clone());
    if !matches!(
        tokio::time::timeout(Duration::from_secs(3), reporter.leader()).await,
        Ok(Ok(true))
    ) {
        return;
    }
    let controls = shared.controls.active.load_full();
    for state in states {
        let Some((namespace, name)) = state.owner.split_once('/') else {
            continue;
        };
        let Some(current) = resources.ingresses.iter().find(|i| {
            i.namespace().as_deref() == Some(namespace)
                && i.name_any() == name
                && i.uid().as_deref() == Some(&state.uid)
        }) else {
            continue;
        };
        let matches_policy = current
            .annotations()
            .get("rgnix.io/traffic-policy")
            .and_then(|v| crate::rollout::Policy::parse(v).ok())
            .is_some_and(|p| {
                serde_json::to_value(p).ok() == serde_json::to_value(&state.policy).ok()
            });
        if !matches_policy {
            continue;
        }
        if state
            .sync_progress(current.annotations().get(PROGRESS).map(String::as_str))
            .is_err()
        {
            continue;
        }
        let gates_pass = {
            let results = shared
                .metric_results
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.policy.metric_gates.iter().all(|name| {
                results.get(name).is_some_and(|(digest, pass, at)| {
                    digest == &controls.digest && *pass && at.elapsed().as_secs() <= 60
                })
            })
        };
        let Some(next) =
            state.next_progress(k8s_openapi::chrono::Utc::now().timestamp(), gates_pass)
        else {
            continue;
        };
        let Ok(value) = serde_json::to_string(&next) else {
            continue;
        };
        let patch = serde_json::json!({"metadata":{"resourceVersion":current.resource_version(),"annotations":{PROGRESS:value}}});
        let api: Api<Ingress> = Api::namespaced(client.clone(), namespace);
        match tokio::time::timeout(
            Duration::from_secs(3),
            api.patch(name, &Default::default(), &kube::api::Patch::Merge(&patch)),
        )
        .await
        {
            Ok(Ok(_)) => {
                let _ = shared.audit.record(
                    "control",
                    &format!("rollout/{}", state.owner),
                    shared.snapshot.load().version,
                    if next.promoted {
                        "promoted"
                    } else {
                        "stage-published"
                    },
                );
            }
            error => log::warn!(
                "rollout progress {} was not published: {error:?}",
                state.owner
            ),
        }
    }
}
