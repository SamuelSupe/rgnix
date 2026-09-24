use super::{Options, Resources};
use anyhow::{Context, Result, ensure};
use futures::{StreamExt, stream};
use k8s_openapi::api::{
    core::v1::ConfigMap,
    networking::v1::{Ingress, IngressSpec},
};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, ListParams, PostParams},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, time::Duration};

const LABEL: &str = "rgnix.io/checkpoint-class";
const DATA: &str = "accepted.json";
const TIMEOUT: Duration = Duration::from_secs(5);
const CONCURRENCY: usize = 8;

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Accepted {
    pub schema: u8,
    pub class: String,
    pub uid: String,
    pub namespace: String,
    pub name: String,
    pub reference: String,
    pub plugin_uid: String,
    pub source: String,
    pub spec: IngressSpec,
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..32].into()
}
pub(super) fn name(class: &str, uid: &str) -> String {
    format!("rgnix-state-{}", digest(&format!("{class}/{uid}")))
}

pub(super) fn restore(
    resources: &Resources,
    options: &Options,
    uid: &str,
    plugin_uid: &str,
) -> Option<Accepted> {
    let expected = name(&options.class, uid);
    let map = resources.maps.iter().find(|map| {
        map.namespace().as_deref() == Some(&options.publish_namespace)
            && map.name_any() == expected
            && map.labels().get(LABEL) == Some(&digest(&options.class))
    })?;
    let data = map.data.as_ref()?.get(DATA)?;
    if data.len() > 900 * 1024 {
        return None;
    }
    let accepted: Accepted = serde_json::from_str(data).ok()?;
    (accepted.schema == 1
        && accepted.uid == uid
        && accepted.class == options.class
        && accepted.plugin_uid == plugin_uid)
        .then_some(accepted)
}

pub(super) async fn persist(
    client: Client,
    options: &Options,
    desired: &BTreeMap<String, Accepted>,
) -> Result<()> {
    let api = Api::<ConfigMap>::namespaced(client.clone(), &options.publish_namespace);
    let label = digest(&options.class);
    let existing = tokio::time::timeout(
        TIMEOUT,
        api.list(&ListParams::default().labels(&format!("{LABEL}={label}"))),
    )
    .await
    .context("listing Ingress checkpoints timed out")??;
    let mut stale: BTreeMap<_, _> = existing
        .into_iter()
        .filter(|map| {
            map.labels().get(LABEL) == Some(&label) && map.name_any().starts_with("rgnix-state-")
        })
        .map(|map| (map.name_any(), map))
        .collect();
    let writes: Vec<_> = desired
        .values()
        .map(|accepted| {
            let key = name(&options.class, &accepted.uid);
            let previous = stale.remove(&key);
            let (client, api, label) = (&client, &api, &label);
            async move {
                let result = tokio::time::timeout(
                    TIMEOUT,
                    persist_one(client, api, options, label, accepted, previous),
                )
                .await
                .context("checkpoint write timed out")
                .and_then(|r| r);
                (key, result)
            }
        })
        .collect();
    let mut failures = 0;
    // A rejected or stalled tenant must not cancel the remaining writes or cleanup.
    let mut writes = stream::iter(writes).buffer_unordered(CONCURRENCY);
    while let Some((key, result)) = writes.next().await {
        if let Err(error) = result {
            failures += 1;
            log::warn!("Ingress checkpoint {key}: {error:#}");
        }
    }
    let mut deletes = stream::iter(stale.into_iter().map(|(key, map)| {
        let (client, api) = (&client, &api);
        async move {
            let result = tokio::time::timeout(TIMEOUT, prune_one(client, api, &key, map))
                .await
                .context("checkpoint cleanup timed out")
                .and_then(|r| r);
            (key, result)
        }
    }))
    .buffer_unordered(CONCURRENCY);
    while let Some((key, result)) = deletes.next().await {
        if let Err(error) = result {
            failures += 1;
            log::warn!("Ingress checkpoint cleanup {key}: {error:#}");
        }
    }
    ensure!(
        failures == 0,
        "{failures} Ingress checkpoint operations failed"
    );
    Ok(())
}

async fn persist_one(
    client: &Client,
    api: &Api<ConfigMap>,
    options: &Options,
    label: &str,
    accepted: &Accepted,
    previous: Option<ConfigMap>,
) -> Result<()> {
    let data = serde_json::to_string(accepted)?;
    ensure!(
        data.len() <= 900 * 1024,
        "accepted Ingress checkpoint exceeds 900 KiB"
    );
    if previous
        .as_ref()
        .and_then(|m| m.data.as_ref())
        .and_then(|d| d.get(DATA))
        == Some(&data)
    {
        return Ok(());
    }
    // Read the checkpoint before verifying its source; replace then uses resourceVersion CAS.
    // This prevents a lagging replica from overwriting a newer accepted configuration.
    let ingress = Api::<Ingress>::namespaced(client.clone(), &accepted.namespace)
        .get_opt(&accepted.name)
        .await?;
    let Some(ingress) = ingress else {
        return Ok(());
    };
    if ingress.uid().as_deref() != Some(&accepted.uid)
        || ingress.spec.as_ref() != Some(&accepted.spec)
        || ingress.annotations().get("rgnix.io/script") != Some(&accepted.reference)
    {
        return Ok(());
    }
    let Some((map_name, map_key)) = accepted.reference.split_once('/') else {
        return Ok(());
    };
    let source = Api::<ConfigMap>::namespaced(client.clone(), &accepted.namespace)
        .get_opt(map_name)
        .await?;
    if !source.is_some_and(|m| {
        m.uid().as_deref() == Some(&accepted.plugin_uid)
            && m.data.as_ref().and_then(|d| d.get(map_key)) == Some(&accepted.source)
    }) {
        return Ok(());
    }
    let key = name(&options.class, &accepted.uid);
    let replacing = previous.is_some();
    let mut map = previous.unwrap_or_default();
    map.metadata.name = Some(key.clone());
    map.metadata.namespace = Some(options.publish_namespace.clone());
    map.metadata.labels = Some(BTreeMap::from([(LABEL.into(), label.into())]));
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

async fn prune_one(client: &Client, api: &Api<ConfigMap>, key: &str, map: ConfigMap) -> Result<()> {
    if let Some(data) = map.data.as_ref().and_then(|d| d.get(DATA))
        && let Ok(accepted) = serde_json::from_str::<Accepted>(data)
        && let Some(ingress) = Api::<Ingress>::namespaced(client.clone(), &accepted.namespace)
            .get_opt(&accepted.name)
            .await?
        && ingress.uid().as_deref() == Some(&accepted.uid)
        && let Some(reference) = ingress.annotations().get("rgnix.io/script")
        && let Some((map_name, map_key)) = reference.split_once('/')
        && let Some(source) = Api::<ConfigMap>::namespaced(client.clone(), &accepted.namespace)
            .get_opt(map_name)
            .await?
        && source.uid().as_deref() == Some(&accepted.plugin_uid)
        && source
            .data
            .as_ref()
            .is_some_and(|d| d.contains_key(map_key))
    {
        // This replica may not have observed the latest Ingress yet.
        return Ok(());
    }
    // Only delete this controller's checkpoints, guarded against concurrent replacement.
    let params = DeleteParams {
        preconditions: Some(kube::api::Preconditions {
            resource_version: map.resource_version(),
            uid: map.uid(),
        }),
        ..Default::default()
    };
    match api.delete(key, &params).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}
