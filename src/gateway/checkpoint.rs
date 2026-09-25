use super::{
    controller::{Options, Resources, resource},
    references::find,
    spec::RouteSpec,
};
use crate::runtime::Shared;
use anyhow::{Result, ensure};
use futures::{StreamExt, stream};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, ListParams, PostParams},
    core::DynamicObject,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

const LABEL: &str = "rgnix.io/gateway-checkpoint";
const DATA: &str = "accepted.json";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Accepted {
    pub schema: u8,
    pub gateway_uid: String,
    pub namespace: String,
    pub name: String,
    pub kind: String,
    pub uid: String,
    pub reference: String,
    pub configmap_uid: String,
    pub source: String,
    pub spec: RouteSpec,
    pub annotations: BTreeMap<String, String>,
}
fn scope(options: &Options) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{}/{}", options.namespace, options.name))
    )[..32]
        .into()
}
fn name(options: &Options, uid: &str) -> String {
    format!(
        "rgnix-gw-{:x}",
        Sha256::digest(format!("{}/{uid}", scope(options)))
    )[..55]
        .into()
}
pub(super) fn restore(
    resources: &Resources,
    options: &Options,
    object: &DynamicObject,
    kind: &str,
) -> Option<Accepted> {
    let uid = object.uid()?;
    let map = find(
        resources,
        "ConfigMap",
        &options.publish_namespace,
        &name(options, &uid),
    )?;
    if map.labels().get(LABEL) != Some(&scope(options)) {
        return None;
    }
    let raw = map.data["data"][DATA].as_str()?;
    if raw.len() > 900 * 1024 {
        return None;
    }
    let accepted: Accepted = serde_json::from_str(raw).ok()?;
    let gateway = find(resources, "Gateway", &options.namespace, &options.name)?;
    (accepted.schema == 1
        && accepted.uid == uid
        && accepted.kind == kind
        && accepted.namespace == object.namespace()?
        && accepted.name == object.name_any()
        && gateway.uid().as_deref() == Some(&accepted.gateway_uid))
    .then_some(accepted)
}
pub(super) fn annotations(object: &DynamicObject) -> BTreeMap<String, String> {
    let mut values = crate::ingress::policy::annotations(object.annotations());
    if let Some(script) = object.annotations().get("rgnix.io/script") {
        values.insert("rgnix.io/script".into(), script.clone());
    }
    values
}

async fn current(client: &Client, accepted: &Accepted) -> Result<Option<DynamicObject>> {
    ensure!(
        matches!(accepted.kind.as_str(), "HTTPRoute" | "GRPCRoute"),
        "invalid checkpoint kind"
    );
    Ok(Api::<DynamicObject>::namespaced_with(
        client.clone(),
        &accepted.namespace,
        &resource(&accepted.kind),
    )
    .get_opt(&accepted.name)
    .await?)
}
async fn write(
    client: &Client,
    options: &Options,
    accepted: &Accepted,
    previous: Option<ConfigMap>,
) -> Result<()> {
    let data = serde_json::to_string(accepted)?;
    ensure!(
        data.len() <= 900 * 1024,
        "Gateway checkpoint exceeds 900 KiB"
    );
    if previous.as_ref().and_then(|m| m.data.as_ref()?.get(DATA)) == Some(&data) {
        return Ok(());
    }
    let Some(route) = current(client, accepted).await? else {
        return Ok(());
    };
    if route.uid().as_deref() != Some(&accepted.uid)
        || serde_json::from_value::<RouteSpec>(route.data["spec"].clone())
            .ok()
            .and_then(|s| serde_json::to_value(s).ok())
            != Some(serde_json::to_value(&accepted.spec)?)
        || annotations(&route) != accepted.annotations
    {
        return Ok(());
    }
    let gateway = Api::<DynamicObject>::namespaced_with(
        client.clone(),
        &options.namespace,
        &resource("Gateway"),
    )
    .get_opt(&options.name)
    .await?;
    if gateway.and_then(|g| g.uid()).as_deref() != Some(&accepted.gateway_uid) {
        return Ok(());
    }
    let (source_name, key) = accepted
        .reference
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid script reference"))?;
    let source = Api::<ConfigMap>::namespaced(client.clone(), &accepted.namespace)
        .get_opt(source_name)
        .await?;
    if !source.is_some_and(|s| {
        s.uid().as_deref() == Some(&accepted.configmap_uid)
            && s.data.as_ref().and_then(|d| d.get(key)) == Some(&accepted.source)
    }) {
        return Ok(());
    }
    let api = Api::<ConfigMap>::namespaced(client.clone(), &options.publish_namespace);
    let key = name(options, &accepted.uid);
    let replacing = previous.is_some();
    let mut map = previous.unwrap_or_default();
    map.metadata.name = Some(key.clone());
    map.metadata.labels = Some(BTreeMap::from([(LABEL.into(), scope(options))]));
    map.data = Some(BTreeMap::from([(DATA.into(), data)]));
    let result = if replacing {
        api.replace(&key, &PostParams::default(), &map).await
    } else {
        api.create(&PostParams::default(), &map).await
    };
    match result {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}
async fn prune(client: &Client, options: &Options, map: ConfigMap) -> Result<()> {
    if let Some(accepted) = map
        .data
        .as_ref()
        .and_then(|d| d.get(DATA))
        .and_then(|s| serde_json::from_str::<Accepted>(s).ok())
        && let Some(route) = current(client, &accepted).await?
        && route.uid().as_deref() == Some(&accepted.uid)
        && route.data["spec"]["parentRefs"]
            .as_array()
            .is_some_and(|parents| {
                parents
                    .iter()
                    .any(|p| super::status::owns_parent(p, &route, options))
            })
        && Api::<DynamicObject>::namespaced_with(
            client.clone(),
            &options.namespace,
            &resource("Gateway"),
        )
        .get_opt(&options.name)
        .await?
        .and_then(|g| g.uid())
        .as_deref()
            == Some(&accepted.gateway_uid)
        && route.annotations().get("rgnix.io/script") == Some(&accepted.reference)
        && let Some((source, key)) = accepted.reference.split_once('/')
        && let Some(source) = Api::<ConfigMap>::namespaced(client.clone(), &accepted.namespace)
            .get_opt(source)
            .await?
        && source.uid().as_deref() == Some(&accepted.configmap_uid)
        && source
            .data
            .as_ref()
            .is_some_and(|data| data.contains_key(key))
    {
        // An older watch snapshot must not delete another replica's accepted source.
        return Ok(());
    }
    let params = DeleteParams {
        preconditions: Some(kube::api::Preconditions {
            resource_version: map.resource_version(),
            uid: map.uid(),
        }),
        ..Default::default()
    };
    match Api::<ConfigMap>::namespaced(client.clone(), &options.publish_namespace)
        .delete(&map.name_any(), &params)
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}
async fn persist(
    client: &Client,
    options: &Options,
    desired: &BTreeMap<String, Accepted>,
) -> Result<()> {
    let api = Api::<ConfigMap>::namespaced(client.clone(), &options.publish_namespace);
    let maps = tokio::time::timeout(
        TIMEOUT,
        api.list(&ListParams::default().labels(&format!("{LABEL}={}", scope(options)))),
    )
    .await??;
    let mut stale: BTreeMap<_, _> = maps
        .into_iter()
        .filter(|m| m.name_any().starts_with("rgnix-gw-"))
        .map(|m| (m.name_any(), m))
        .collect();
    let writes: Vec<_> = desired
        .values()
        .map(|accepted| {
            let previous = stale.remove(&name(options, &accepted.uid));
            async move {
                tokio::time::timeout(TIMEOUT, write(client, options, accepted, previous)).await
            }
        })
        .collect();
    let mut failed = 0;
    let mut writes = stream::iter(writes).buffer_unordered(8);
    while let Some(result) = writes.next().await {
        if !matches!(result, Ok(Ok(()))) {
            failed += 1;
            log::warn!("Gateway checkpoint write: {result:?}");
        }
    }
    let mut deletes = stream::iter(stale.into_values().map(|map| async move {
        tokio::time::timeout(TIMEOUT, prune(client, options, map)).await
    }))
    .buffer_unordered(8);
    while let Some(result) = deletes.next().await {
        if !matches!(result, Ok(Ok(()))) {
            failed += 1;
            log::warn!("Gateway checkpoint cleanup: {result:?}");
        }
    }
    ensure!(failed == 0, "{failed} Gateway checkpoint operations failed");
    Ok(())
}
pub(super) async fn run(
    client: Client,
    options: Options,
    mut desired: tokio::sync::watch::Receiver<BTreeMap<String, Accepted>>,
    shared: Arc<Shared>,
    mut shutdown: pingora::server::ShutdownWatch,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! { _=shutdown.changed()=>break, r=desired.changed()=>if r.is_err(){break;}, _=tick.tick()=>{} }
        let desired = desired.borrow_and_update().clone();
        let result =
            tokio::select! { _=shutdown.changed()=>break, r=persist(&client,&options,&desired)=>r };
        shared
            .telemetry
            .checkpoint_healthy
            .set(i64::from(result.is_ok()));
        if let Err(error) = result {
            shared.telemetry.checkpoint_errors.inc();
            log::warn!("Gateway checkpoint: {error:#}");
        }
    }
}
