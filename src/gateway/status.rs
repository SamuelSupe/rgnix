use super::{
    CONTROLLER,
    controller::{Options, resource},
};
use crate::runtime::Shared;
use anyhow::Result;
use k8s_openapi::{
    api::{
        coordination::v1::{Lease, LeaseSpec},
        core::v1::Service,
    },
    apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta},
    chrono::{TimeDelta, Utc},
};
use kube::{
    Api, Client, ResourceExt,
    api::{Patch, PatchParams, PostParams},
    core::DynamicObject,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
pub(super) struct Update {
    pub kind: String,
    pub object: Arc<DynamicObject>,
    pub status: Value,
}
impl Update {
    pub fn new(kind: &str, object: &Arc<DynamicObject>, status: Value) -> Self {
        Self {
            kind: kind.into(),
            object: object.clone(),
            status,
        }
    }
}
pub(super) fn condition(
    object: &DynamicObject,
    type_: &str,
    status: bool,
    reason: &str,
    message: &str,
) -> Value {
    json!({"type":type_,"status":if status {"True"} else {"False"},"reason":reason,"message":message.chars().take(2048).collect::<String>(),"observedGeneration":object.metadata.generation.unwrap_or(1),"lastTransitionTime":Utc::now().to_rfc3339_opts(k8s_openapi::chrono::SecondsFormat::Secs, true)})
}

fn stable_conditions(new: &mut Value, old: &Value) {
    if let Some(conditions) = new["conditions"].as_array_mut() {
        for condition in conditions {
            if let Some(previous) = old["conditions"].as_array().and_then(|cs| {
                cs.iter()
                    .find(|c| c["type"] == condition["type"] && c["status"] == condition["status"])
            }) {
                condition["lastTransitionTime"] = previous["lastTransitionTime"].clone();
            }
        }
    }
    for (array, key) in [
        ("listeners", "name"),
        ("parents", "parentRef"),
        ("ancestors", "ancestorRef"),
    ] {
        if let Some(entries) = new[array].as_array_mut() {
            for entry in entries {
                if let Some(previous) = old[array]
                    .as_array()
                    .and_then(|items| items.iter().find(|item| item[key] == entry[key]))
                {
                    stable_conditions(entry, previous);
                }
            }
        }
    }
}

async fn leader(client: &Client, options: &Options, shared: &Shared) -> Result<bool> {
    let api = Api::<Lease>::namespaced(client.clone(), &options.namespace);
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(options.name.as_bytes()));
    let name = format!(
        "{}-gw-{}",
        options.name.chars().take(40).collect::<String>(),
        &digest[..12]
    );
    let current = api.get_opt(&name).await?;
    let now = Utc::now();
    if let Some(spec) = current.as_ref().and_then(|l| l.spec.as_ref())
        && spec.holder_identity.as_deref() != Some(&options.identity)
        && spec.renew_time.as_ref().is_some_and(|t| {
            t.0 + TimeDelta::seconds(i64::from(spec.lease_duration_seconds.unwrap_or(30))) > now
        })
    {
        return Ok(false);
    }
    let mut lease = current.clone().unwrap_or_else(|| Lease {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            ..Default::default()
        },
        ..Default::default()
    });
    lease.spec = Some(LeaseSpec {
        holder_identity: Some(options.identity.clone()),
        lease_duration_seconds: Some(30),
        renew_time: Some(MicroTime(now)),
        ..Default::default()
    });
    let result = if current.is_some() {
        api.replace(&name, &PostParams::default(), &lease).await
    } else {
        api.create(&PostParams::default(), &lease).await
    };
    match result {
        Ok(_) => {
            shared
                .telemetry
                .controller
                .leader_until
                .store(now.timestamp() + 30, std::sync::atomic::Ordering::Relaxed);
            Ok(true)
        }
        Err(kube::Error::Api(error)) if error.code == 409 => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn owns_parent(parent: &Value, object: &DynamicObject, options: &Options) -> bool {
    parent["name"] == options.name
        && parent["namespace"]
            .as_str()
            .unwrap_or(&object.namespace().unwrap_or_default())
            == options.namespace
        && parent["group"].as_str().unwrap_or(super::GROUP) == super::GROUP
        && parent["kind"].as_str().unwrap_or("Gateway") == "Gateway"
}

async fn publish(
    client: &Client,
    update: &Update,
    addresses: &Value,
    options: &Options,
) -> Result<()> {
    let ar = resource(&update.kind);
    let api: Api<DynamicObject> = if let Some(ns) = update.object.namespace() {
        Api::namespaced_with(client.clone(), &ns, &ar)
    } else {
        Api::all_with(client.clone(), &ar)
    };
    let current = api.get(&update.object.name_any()).await?;
    if current.uid() != update.object.uid()
        || current.metadata.generation != update.object.metadata.generation
    {
        return Ok(());
    }
    let mut status = update.status.clone();
    if update.kind == "Gateway" {
        status["addresses"] = addresses.clone();
    }
    if update.kind.ends_with("Route") || update.kind == "BackendTLSPolicy" {
        let (array, reference) = if update.kind == "BackendTLSPolicy" {
            ("ancestors", "ancestorRef")
        } else {
            ("parents", "parentRef")
        };
        // Merge this controller's parent entries while retaining other implementations' status.
        let desired = status[array].as_array_mut().unwrap();
        if let Some(parents) = current.data["status"][array].as_array() {
            desired.extend(
                parents
                    .iter()
                    .filter(|p| {
                        p["controllerName"] != CONTROLLER
                            || !owns_parent(&p[reference], &current, options)
                    })
                    .cloned(),
            );
        }
    }
    stable_conditions(&mut status, &current.data["status"]);
    if status == current.data["status"] {
        return Ok(());
    }
    let patch = json!({"metadata":{"resourceVersion":current.resource_version()},"status":status});
    api.patch_status(
        &current.name_any(),
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

pub(super) async fn run(
    client: Client,
    options: Options,
    mut reports: tokio::sync::watch::Receiver<Vec<Update>>,
    shared: Arc<Shared>,
    mut shutdown: pingora::server::ShutdownWatch,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! { _=shutdown.changed()=>break, result=reports.changed()=>if result.is_err(){break;}, _=ticker.tick()=>{} }
        let updates = reports.borrow_and_update().clone();
        let result =
            tokio::time::timeout(Duration::from_secs(5), leader(&client, &options, &shared)).await;
        let outcome = match result {
            Ok(Ok(true)) => "acquired",
            Ok(Ok(false)) => "contended",
            Ok(Err(_)) => "error",
            Err(_) => "timeout",
        };
        shared
            .telemetry
            .controller
            .lease_results
            .with_label_values(&[outcome])
            .inc();
        if outcome != "acquired" {
            shared
                .telemetry
                .controller
                .leader_until
                .store(0, std::sync::atomic::Ordering::Relaxed);
            for update in &updates {
                if matches!(update.kind.as_str(), "HTTPRoute" | "GRPCRoute")
                    && let Err(error) = crate::ingress::release::reconcile_object(
                        &client,
                        &shared,
                        &update.kind,
                        &update.object,
                        false,
                    )
                    .await
                {
                    log::warn!("Gateway rollback persistence: {error:#}");
                }
            }
            continue;
        }
        let service = tokio::time::timeout(
            Duration::from_secs(5),
            Api::<Service>::namespaced(client.clone(), &options.publish_namespace)
                .get(&options.publish_service),
        )
        .await;
        let addresses = if let Ok(Ok(service)) = service {
            json!(
                service
                    .status
                    .and_then(|s| s.load_balancer)
                    .and_then(|l| l.ingress)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|a| a
                        .ip
                        .map(|ip| json!({"type":"IPAddress","value":ip}))
                        .or_else(|| a
                            .hostname
                            .map(|host| json!({"type":"Hostname","value":host}))))
                    .collect::<Vec<_>>()
            )
        } else {
            json!([])
        };
        let mut renewed = std::time::Instant::now();
        for update in updates {
            if *shutdown.borrow() {
                return;
            }
            if renewed.elapsed() >= Duration::from_secs(10) {
                if !matches!(
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        leader(&client, &options, &shared)
                    )
                    .await,
                    Ok(Ok(true))
                ) {
                    break;
                }
                renewed = std::time::Instant::now();
            }
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                publish(&client, &update, &addresses, &options),
            )
            .await;
            if !matches!(result, Ok(Ok(()))) {
                shared.telemetry.report_errors.inc();
                log::warn!(
                    "Gateway status {} {}: {result:?}",
                    update.kind,
                    update.object.name_any()
                );
            }
            if matches!(update.kind.as_str(), "HTTPRoute" | "GRPCRoute")
                && let Err(error) = crate::ingress::release::reconcile_object(
                    &client,
                    &shared,
                    &update.kind,
                    &update.object,
                    true,
                )
                .await
            {
                log::warn!("Gateway rollout progress: {error:#}");
            }
        }
    }
}
