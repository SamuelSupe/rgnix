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
    pub kind: Option<String>,
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
    let states: Vec<_> = shared
        .rollouts
        .active()
        .into_iter()
        .filter(|s| {
            s.owner == command.owner
                && s.policy.revision == command.revision
                && command.kind.as_ref().is_none_or(|kind| kind == &s.kind)
        })
        .collect();
    ensure!(
        states.len() == 1,
        "rollout revision not found or ambiguous; specify kind"
    );
    let state = &states[0];
    let (namespace, name) = command
        .owner
        .split_once('/')
        .context("owner must be namespace/name")?;
    let api = api(Client::try_default().await?, namespace, &state.kind)?;
    let current = tokio::time::timeout(Duration::from_secs(5), api.get(name)).await??;
    ensure!(
        current.uid().as_deref() == Some(&state.uid),
        "rollout owner changed"
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
        .unwrap_or_else(|| initial(state));
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

fn api(client: Client, namespace: &str, kind: &str) -> Result<Api<kube::core::DynamicObject>> {
    let group = match kind {
        "Ingress" => "networking.k8s.io",
        "HTTPRoute" | "GRPCRoute" => "gateway.networking.k8s.io",
        _ => anyhow::bail!("unsupported rollout resource kind"),
    };
    let resource =
        kube::core::ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(group, "v1", kind));
    Ok(Api::namespaced_with(client, namespace, &resource))
}

pub(crate) async fn reconcile_object(
    client: &Client,
    shared: &Shared,
    kind: &str,
    current: &kube::core::DynamicObject,
    leader: bool,
) -> Result<()> {
    let namespace = current.namespace().unwrap_or_default();
    let name = current.name_any();
    let policy = current
        .annotations()
        .get("rgnix.io/traffic-policy")
        .and_then(|v| crate::rollout::Policy::parse(v).ok());
    let Some(policy) = policy else {
        return Ok(());
    };
    let Some(state) = shared.rollouts.active().into_iter().find(|state| {
        state.kind == kind
            && state.owner == format!("{namespace}/{name}")
            && current.uid().as_deref() == Some(&state.uid)
            && serde_json::to_value(&state.policy).ok() == serde_json::to_value(&policy).ok()
    }) else {
        return Ok(());
    };
    let (key, value, outcome) = if state.rolled_back() {
        if current.annotations().get("rgnix.io/rolled-back-revision") == Some(&policy.revision) {
            return Ok(());
        }
        (
            "rgnix.io/rolled-back-revision",
            policy.revision,
            "rolled-back",
        )
    } else {
        if !leader || policy.steps.is_empty() {
            return Ok(());
        }
        state.sync_progress(current.annotations().get(PROGRESS).map(String::as_str))?;
        let controls = shared.controls.active.load_full();
        let gates_pass = {
            let results = shared
                .metric_results
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            policy.metric_gates.iter().all(|name| {
                results
                    .get(name)
                    .is_some_and(|r| r.passed(&controls.digest))
            })
        };
        let Some(next) =
            state.next_progress(k8s_openapi::chrono::Utc::now().timestamp(), gates_pass)
        else {
            return Ok(());
        };
        (
            PROGRESS,
            serde_json::to_string(&next)?,
            if next.promoted {
                "promoted"
            } else {
                "stage-published"
            },
        )
    };
    let patch = serde_json::json!({"metadata":{"resourceVersion":current.resource_version(),"annotations":{key:value}}});
    tokio::time::timeout(
        Duration::from_secs(3),
        api(client.clone(), &namespace, kind)?.patch(
            &name,
            &Default::default(),
            &kube::api::Patch::Merge(&patch),
        ),
    )
    .await??;
    let _ = shared.audit.record(
        "control",
        &format!("rollout/{kind}/{}", state.owner),
        shared.snapshot.load().version,
        outcome,
    );
    Ok(())
}

pub(super) async fn reconcile(
    client: &Client,
    shared: &Shared,
    options: &Options,
    resources: &Resources,
) {
    if shared.rollouts.active().is_empty() {
        return;
    }
    let reporter =
        super::status::Reporter::new(client.clone(), options.clone(), shared.telemetry.clone());
    let leader = matches!(reporter.leader(Duration::from_secs(3)).await, Ok(true));
    let mut renewed = std::time::Instant::now();
    for ingress in &resources.ingresses {
        if renewed.elapsed() >= Duration::from_secs(10) {
            if !matches!(reporter.leader(Duration::from_secs(3)).await, Ok(true)) {
                break;
            }
            renewed = std::time::Instant::now();
        }
        let current = serde_json::to_value(ingress).and_then(serde_json::from_value);
        if let Ok(current) = current
            && let Err(error) = reconcile_object(client, shared, "Ingress", &current, leader).await
        {
            log::warn!(
                "rollout progress {} was not published: {error:#}",
                ingress.name_any()
            );
        }
    }
}
