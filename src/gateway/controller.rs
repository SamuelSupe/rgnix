use super::{GROUP, build, checkpoint, status};
use crate::runtime::Shared;
use anyhow::Result;
use futures::StreamExt;
use kube::{
    Api, Client, ResourceExt,
    core::{ApiResource, DynamicObject, GroupVersionKind},
    runtime::{WatchStreamExt, watcher},
};
use pingora::server::ShutdownWatch;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone)]
pub struct Options {
    pub namespace: String,
    pub name: String,
    pub namespaces: Vec<String>,
    pub publish_namespace: String,
    pub publish_service: String,
    pub identity: String,
    pub http_port: u16,
    pub https_port: u16,
    pub identity_policy: crate::identity::IdentityPolicy,
}

pub(super) fn resource(kind: &str) -> ApiResource {
    let (group, version) = match kind {
        "Service" | "Secret" | "ConfigMap" | "Namespace" => ("", "v1"),
        "EndpointSlice" => ("discovery.k8s.io", "v1"),
        _ => (GROUP, "v1"),
    };
    ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind))
}
pub(super) type Resources = BTreeMap<String, Vec<Arc<DynamicObject>>>;
#[derive(Default)]
struct Cache {
    ready: bool,
    objects: BTreeMap<(String, String), Arc<DynamicObject>>,
}
fn key(object: &DynamicObject) -> (String, String) {
    (object.namespace().unwrap_or_default(), object.name_any())
}

fn observe(
    client: Client,
    kind: &str,
    namespace: Option<&str>,
    changed: watch::Sender<()>,
    mut shutdown: ShutdownWatch,
    shared: Arc<Shared>,
) -> Arc<Mutex<Cache>> {
    let resource = resource(kind);
    let api = namespace.map_or_else(
        || Api::all_with(client.clone(), &resource),
        |ns| Api::namespaced_with(client.clone(), ns, &resource),
    );
    let cache = Arc::new(Mutex::new(Cache::default()));
    let output = cache.clone();
    let kind = kind.to_owned();
    tokio::spawn(async move {
        let events = watcher(api, watcher::Config::default()).default_backoff();
        tokio::pin!(events);
        let mut staging = BTreeMap::new();
        let mut status = shared.telemetry.controller.watch(&kind);
        loop {
            tokio::select! {
                _=shutdown.changed()=>break,
                event=events.next()=>match event {
                    Some(Ok(event)) => {
                        status.healthy(true);
                        let mut cache = output.lock().unwrap_or_else(|e| e.into_inner());
                        let name = match event {
                            watcher::Event::Init => { staging.clear(); status.synchronized(false); "init" },
                            watcher::Event::InitApply(o) => { staging.insert(key(&o), Arc::new(o)); "init_apply" },
                            watcher::Event::InitDone => { cache.objects = std::mem::take(&mut staging); cache.ready = true; status.synchronized(true); "init_done" },
                            watcher::Event::Apply(o) => { cache.objects.insert(key(&o), Arc::new(o)); "apply" },
                            watcher::Event::Delete(o) => { cache.objects.remove(&key(&o)); "delete" },
                        };
                        shared.telemetry.controller.watch_events.with_label_values(&[kind.as_str(), name]).inc();
                        shared.telemetry.controller.watch_last_event.with_label_values(&[&kind]).set(k8s_openapi::chrono::Utc::now().timestamp());
                        if matches!(name, "init_done" | "apply" | "delete") { let _ = changed.send(()); }
                    },
                    Some(Err(error)) => {
                        status.healthy(false);
                        shared.telemetry.controller.watch_errors.with_label_values(&[&kind]).inc();
                        log::warn!("Gateway watch {kind}: {error}");
                    },
                    None => break,
                }
            }
        }
    });
    cache
}

pub async fn run(shared: Arc<Shared>, options: Options, mut shutdown: ShutdownWatch) -> Result<()> {
    let client = Client::try_default().await?;
    crate::fleet::start(
        shared.clone(),
        client.clone(),
        options.publish_namespace.clone(),
        options.publish_service.clone(),
        options.identity.clone(),
        format!("gateway:{}/{}", options.namespace, options.name),
        shutdown.clone(),
    );
    let (changed, mut changes) = watch::channel(());
    let mut stores = vec![];
    for kind in ["GatewayClass", "Namespace"] {
        stores.push((
            kind,
            observe(
                client.clone(),
                kind,
                None,
                changed.clone(),
                shutdown.clone(),
                shared.clone(),
            ),
        ));
    }
    stores.push((
        "Gateway",
        observe(
            client.clone(),
            "Gateway",
            Some(&options.namespace),
            changed.clone(),
            shutdown.clone(),
            shared.clone(),
        ),
    ));
    for kind in [
        "HTTPRoute",
        "GRPCRoute",
        "ReferenceGrant",
        "BackendTLSPolicy",
        "Service",
        "EndpointSlice",
        "Secret",
        "ConfigMap",
    ] {
        if options.namespaces.is_empty() {
            stores.push((
                kind,
                observe(
                    client.clone(),
                    kind,
                    None,
                    changed.clone(),
                    shutdown.clone(),
                    shared.clone(),
                ),
            ));
        } else {
            let mut namespaces = options.namespaces.clone();
            namespaces.push(options.namespace.clone());
            namespaces.push(options.publish_namespace.clone());
            namespaces.sort();
            namespaces.dedup();
            for ns in namespaces {
                stores.push((
                    kind,
                    observe(
                        client.clone(),
                        kind,
                        Some(&ns),
                        changed.clone(),
                        shutdown.clone(),
                        shared.clone(),
                    ),
                ));
            }
        }
    }
    let (reports, report_rx) = watch::channel(Vec::new());
    let (checkpoints, checkpoint_rx) = watch::channel(BTreeMap::new());
    tokio::spawn(checkpoint::run(
        client.clone(),
        options.clone(),
        checkpoint_rx,
        shared.clone(),
        shutdown.clone(),
    ));
    tokio::spawn(status::run(
        client.clone(),
        options.clone(),
        report_rx,
        shared.clone(),
        shutdown.clone(),
    ));
    let mut history = build::History::default();
    let mut previous_diagnostics = std::collections::BTreeSet::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            _=changes.changed()=>{},
            _=shared.compiler.changed.notified()=>{},
            _=ticker.tick()=>{},
        }
        let mut resources = Resources::new();
        let mut ready = true;
        for (kind, store) in &stores {
            let cache = store.lock().unwrap_or_else(|e| e.into_inner());
            ready &= cache.ready;
            resources
                .entry((*kind).into())
                .or_default()
                .extend(cache.objects.values().cloned());
        }
        if !ready {
            continue;
        }
        for (kind, resources) in &resources {
            shared
                .telemetry
                .controller
                .resources
                .with_label_values(&[kind])
                .set(resources.len() as i64);
        }
        let started = std::time::Instant::now();
        if shared.fleet.enabled {
            shared.fleet.begin(crate::fleet::source_digest(
                &resources,
                &shared.controls.active.load().digest,
            )?);
        }
        *shared
            .gateway_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(super::preflight::PreviewInput {
            resources: resources.clone(),
            options: options.clone(),
            history: history.clone(),
        });
        let state = shared.clone();
        let opts = options.clone();
        let (built, next_history) =
            tokio::task::spawn_blocking(move || build::build(&resources, &opts, &state, history))
                .await?;
        history = next_history;
        let _ = checkpoints.send(history.checkpoints());
        let mut diagnostics = std::collections::BTreeSet::new();
        for update in &built.updates {
            collect_diagnostics(
                &update.status,
                &format!(
                    "{} {}/{} generation {}",
                    update.kind,
                    update.object.namespace().unwrap_or_default(),
                    update.object.name_any(),
                    update.object.metadata.generation.unwrap_or_default()
                ),
                &mut diagnostics,
            );
        }
        shared
            .telemetry
            .config_degraded
            .set(diagnostics.len() as i64);
        for diagnostic in diagnostics.difference(&previous_diagnostics) {
            shared.telemetry.reload_errors.inc();
            shared.rejected_update(diagnostic);
            log::warn!("Gateway configuration: {diagnostic}");
        }
        previous_diagnostics = diagnostics;
        let hash = built.snapshot.fingerprint()?;
        if shared.snapshot.load().content_hash != hash
            || shared.snapshot.load().ready != built.snapshot.ready
        {
            shared.publish(built.snapshot)?;
            shared
                .telemetry
                .config_update_seconds
                .with_label_values(&["gateway", "published"])
                .observe(started.elapsed().as_secs_f64());
        }
        let _ = reports.send(built.updates);
        shared
            .fleet
            .complete(previous_diagnostics.iter().next().cloned());
    }
    shared.telemetry.ready.store(false, Ordering::Release);
    Ok(())
}

fn collect_diagnostics(
    value: &serde_json::Value,
    resource: &str,
    output: &mut std::collections::BTreeSet<String>,
) {
    if let Some(object) = value.as_object() {
        if object.get("status").is_some_and(|s| s == "False")
            || object
                .get("reason")
                .is_some_and(|r| r == "UsingLastValidConfiguration")
        {
            output.insert(format!(
                "{resource}: {} {}",
                value["reason"].as_str().unwrap_or_default(),
                value["message"].as_str().unwrap_or_default()
            ));
        }
        for child in object.values() {
            collect_diagnostics(child, resource, output);
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            collect_diagnostics(child, resource, output);
        }
    }
}
