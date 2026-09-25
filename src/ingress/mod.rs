mod preflight;
pub(crate) use preflight::{PreviewInput, managed, validate};
mod checkpoint;
mod policy;
pub(crate) mod release;
mod resources;
mod status;
use crate::{model::*, runtime::Shared, script::CompiledScript};
use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use k8s_openapi::api::{
    core::v1::{ConfigMap, Secret, Service},
    discovery::v1::EndpointSlice,
    networking::v1::{Ingress, IngressClass, IngressServiceBackend, IngressSpec},
};
use kube::{
    Api, Client, Resource, ResourceExt,
    runtime::{
        reflector::{self, Store},
        watcher,
    },
};
use serde::de::DeserializeOwned;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::watch;

pub const CONTROLLER: &str = "rgnix.io/ingress-controller";
#[derive(Clone)]
pub struct Options {
    pub class: String,
    pub namespaces: Vec<String>,
    pub publish_namespace: String,
    pub publish_service: String,
    pub identity: String,
    pub identity_policy: crate::identity::IdentityPolicy,
}
struct Stores {
    ingresses: Vec<Store<Ingress>>,
    classes: Store<IngressClass>,
    services: Vec<Store<Service>>,
    slices: Vec<Store<EndpointSlice>>,
    secrets: Vec<Store<Secret>>,
    maps: Vec<Store<ConfigMap>>,
}
#[derive(Clone)]
struct Resources {
    ingresses: Vec<Arc<Ingress>>,
    classes: Vec<Arc<IngressClass>>,
    services: Vec<Arc<Service>>,
    slices: Vec<Arc<EndpointSlice>>,
    secrets: Vec<Arc<Secret>>,
    maps: Vec<Arc<ConfigMap>>,
}
#[derive(Default, Clone)]
struct History {
    scripts: BTreeMap<String, Arc<CompiledScript>>,
    routes: BTreeMap<String, IngressSpec>,
    accepted: BTreeMap<String, checkpoint::Accepted>,
    proposals: BTreeMap<String, (checkpoint::Accepted, Arc<CompiledScript>)>,
    revoked: BTreeSet<String>,
}
#[derive(Clone)]
pub(super) struct Diagnostic {
    pub ingress: Arc<Ingress>,
    pub reason: &'static str,
    pub message: String,
}

fn observe<K>(
    api: Api<K>,
    changed: watch::Sender<()>,
    mut shutdown: pingora::server::ShutdownWatch,
) -> Store<K>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Send + Sync + 'static,
{
    let (store, writer) = reflector::store::<K>();
    tokio::spawn(async move {
        let events = watcher(api, watcher::Config::default());
        let events = reflector::reflector(writer, events);
        tokio::pin!(events);
        loop {
            tokio::select! {
                _=shutdown.changed()=>break,
                event=events.next()=>match event {
                    Some(Ok(watcher::Event::Apply(_)|watcher::Event::Delete(_)|watcher::Event::InitDone))=>{let _=changed.send(());},
                    Some(Ok(_))=>{},Some(Err(e))=>{log::warn!("Kubernetes watch {}: {e}",K::kind(&()));tokio::time::sleep(Duration::from_secs(1)).await;},None=>break,
                }
            }
        }
    });
    store
}
fn observe_namespaced<K>(
    client: Client,
    namespaces: &[String],
    changed: watch::Sender<()>,
    shutdown: pingora::server::ShutdownWatch,
) -> Vec<Store<K>>
where
    K: Clone
        + Debug
        + DeserializeOwned
        + Resource<DynamicType = (), Scope = k8s_openapi::NamespaceResourceScope>
        + Send
        + Sync
        + 'static,
{
    if namespaces.is_empty() {
        return vec![observe(Api::all(client), changed, shutdown)];
    }
    namespaces
        .iter()
        .map(|ns| {
            observe(
                Api::namespaced(client.clone(), ns),
                changed.clone(),
                shutdown.clone(),
            )
        })
        .collect()
}
impl Stores {
    async fn ready(&self) -> Result<()> {
        tokio::try_join!(
            futures::future::try_join_all(self.ingresses.iter().map(|s| s.wait_until_ready())),
            self.classes.wait_until_ready(),
            futures::future::try_join_all(self.services.iter().map(|s| s.wait_until_ready())),
            futures::future::try_join_all(self.slices.iter().map(|s| s.wait_until_ready())),
            futures::future::try_join_all(self.secrets.iter().map(|s| s.wait_until_ready())),
            futures::future::try_join_all(self.maps.iter().map(|s| s.wait_until_ready()))
        )?;
        Ok(())
    }
    fn snapshot(&self) -> Resources {
        Resources {
            ingresses: self.ingresses.iter().flat_map(|s| s.state()).collect(),
            classes: self.classes.state(),
            services: self.services.iter().flat_map(|s| s.state()).collect(),
            slices: self.slices.iter().flat_map(|s| s.state()).collect(),
            secrets: self.secrets.iter().flat_map(|s| s.state()).collect(),
            maps: self.maps.iter().flat_map(|s| s.state()).collect(),
        }
    }
}

pub async fn run(
    shared: Arc<Shared>,
    options: Options,
    mut shutdown: pingora::server::ShutdownWatch,
) -> Result<()> {
    let client = Client::try_default().await?;
    let (tx, mut changed) = watch::channel(());
    let mut with_controller = options.namespaces.clone();
    if !with_controller.is_empty() && !with_controller.contains(&options.publish_namespace) {
        with_controller.push(options.publish_namespace.clone());
    }
    let stores = Stores {
        ingresses: observe_namespaced(
            client.clone(),
            &options.namespaces,
            tx.clone(),
            shutdown.clone(),
        ),
        classes: observe(Api::all(client.clone()), tx.clone(), shutdown.clone()),
        services: observe_namespaced(
            client.clone(),
            &with_controller,
            tx.clone(),
            shutdown.clone(),
        ),
        slices: observe_namespaced(
            client.clone(),
            &options.namespaces,
            tx.clone(),
            shutdown.clone(),
        ),
        secrets: observe_namespaced(
            client.clone(),
            &options.namespaces,
            tx.clone(),
            shutdown.clone(),
        ),
        maps: observe_namespaced(client.clone(), &with_controller, tx, shutdown.clone()),
    };
    tokio::select! {_=shutdown.changed()=>return Ok(()),ready=stores.ready()=>ready?}
    let mut history = History::default();
    let (report_tx, report_rx) = watch::channel((vec![], vec![]));
    tokio::spawn(status::run(
        client.clone(),
        options.clone(),
        report_rx,
        shared.telemetry.clone(),
        shutdown.clone(),
    ));
    let (accepted_tx, mut accepted_rx) = watch::channel(BTreeMap::new());
    let persist_options = options.clone();
    let persist_telemetry = shared.telemetry.clone();
    let mut persist_shutdown = shutdown.clone();
    let checkpoint_client = client.clone();
    tokio::spawn(async move {
        let client = checkpoint_client;
        // Wait for a complete initial reconciliation before deleting obsolete checkpoints.
        if accepted_rx.changed().await.is_err() {
            return;
        }
        let mut ticker = tokio::time::interval(Duration::from_secs(10));
        loop {
            let accepted = accepted_rx.borrow_and_update().clone();
            let result = tokio::select! {
                _=persist_shutdown.changed()=>break,
                result=accepted_rx.changed()=>{
                    if result.is_err() { break; }
                    continue;
                },
                result=checkpoint::persist(client.clone(), &persist_options, &accepted)=>result,
            };
            let failed = result.is_err();
            persist_telemetry.checkpoint_healthy.set(i64::from(!failed));
            if failed {
                persist_telemetry.checkpoint_errors.inc();
                log::warn!("Ingress checkpoint persistence: {result:?}");
            }
            tokio::select! {
                _=persist_shutdown.changed()=>break,
                result=accepted_rx.changed()=>if result.is_err(){break;},
                _=ticker.tick()=>{},
            }
        }
    });
    let mut bootstrapped = false;
    let mut checkpoint_state_sent = false;
    let mut previous_input = None;
    let mut release_check = std::time::Instant::now() - Duration::from_secs(5);
    loop {
        let mut resources = stores.snapshot();
        if release_check.elapsed() >= Duration::from_secs(5) {
            release::reconcile(&client, &shared, &options, &resources).await;
            release_check = std::time::Instant::now();
        }
        for (owner, uid, revision) in shared.rollouts.pending() {
            let Some((ns, name)) = owner.split_once('/') else {
                continue;
            };
            if let Some(ingress) = resources.ingresses.iter().find(|i| {
                i.namespace().as_deref() == Some(ns)
                    && i.name_any() == name
                    && i.uid().as_deref() == Some(&uid)
            }) && ingress.annotations().get("rgnix.io/rolled-back-revision") != Some(&revision)
                && ingress
                    .annotations()
                    .get("rgnix.io/traffic-policy")
                    .and_then(|v| crate::rollout::Policy::parse(v).ok())
                    .is_some_and(|p| p.revision == revision)
            {
                let patch = serde_json::json!({"metadata":{"resourceVersion":ingress.resource_version(),"annotations":{"rgnix.io/rolled-back-revision":revision}}});
                let api: Api<Ingress> = Api::namespaced(client.clone(), ns);
                if let Err(e) = api
                    .patch(name, &Default::default(), &kube::api::Patch::Merge(&patch))
                    .await
                {
                    log::warn!("persist traffic rollback {owner}: {e}");
                }
            }
        }
        *shared
            .ingress_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(PreviewInput {
            options: options.clone(),
            resources: resources.clone(),
            history: history.clone(),
        });
        let input = (
            resources.prepare(&options, &history),
            shared.controls.active.load().digest.clone(),
        );
        if previous_input.as_ref() == Some(&input) {
            tokio::select! { _=shutdown.changed()=>return Ok(()), _=changed.changed()=>{}, _=tokio::time::sleep(Duration::from_secs(1))=>{} }
            continue;
        }
        previous_input = Some(input);
        let options_copy = options.clone();
        let state = shared.clone();
        let result = tokio::task::spawn_blocking(move || {
            build(&state, &options_copy, resources, history, false)
        })
        .await?;
        history = result.1;
        let (mut snapshot, diagnostics, selected) = result.0?;
        let cold_invalid = diagnostics
            .iter()
            .any(|d| d.reason == "UnrecoverablePlugin");
        let has_accepted_route = snapshot
            .hosts
            .iter()
            .flat_map(|h| &h.routes)
            .any(|r| !matches!(r.action, Action::Unavailable));
        bootstrapped |= !cold_invalid || has_accepted_route;
        snapshot.ready = bootstrapped;
        shared
            .telemetry
            .config_degraded
            .set(diagnostics.len() as i64);
        shared.publish(snapshot)?;
        let _ = report_tx.send((diagnostics, selected));
        let mut desired = history.accepted.clone();
        for (uid, (proposal, _)) in &history.proposals {
            desired.insert(uid.clone(), proposal.clone());
        }
        accepted_tx.send_if_modified(|current| {
            if checkpoint_state_sent && *current == desired {
                false
            } else {
                *current = desired;
                checkpoint_state_sent = true;
                true
            }
        });
        tokio::select! {
            _=shutdown.changed()=>{
                shared.telemetry.ready.store(false, Ordering::Release);
                return Ok(());
            },
            _=changed.changed()=>{tokio::time::sleep(Duration::from_millis(100)).await;},
            _=tokio::time::sleep(Duration::from_secs(1))=>{},
        }
    }
}

type Built = Result<(RuntimeSnapshot, Vec<Diagnostic>, Vec<Arc<Ingress>>)>;
fn build(
    shared: &Shared,
    options: &Options,
    mut resources: Resources,
    mut history: History,
    preview: bool,
) -> (Built, History) {
    let result = (|| -> Built {
        let listeners = shared.snapshot.load().listeners.clone();
        let control_policy = shared.controls.active.load_full();
        let tenant_policy = control_policy.tenant_policy.as_ref();
        let mut domain_owners = BTreeMap::new();
        let mut snapshot = RuntimeSnapshot::empty(listeners.clone());
        let mut diagnostics = vec![];
        let mut selected = vec![];
        let Some(class) = resources.classes.iter().find(|c| {
            c.name_any() == options.class
                && c.spec
                    .as_ref()
                    .is_some_and(|s| s.controller.as_deref() == Some(CONTROLLER))
        }) else {
            history.scripts.clear();
            history.routes.clear();
            history.accepted.clear();
            history.proposals.clear();
            return Ok((snapshot, diagnostics, selected));
        };
        let default = class
            .annotations()
            .get("ingressclass.kubernetes.io/is-default-class")
            .is_some_and(|s| s == "true");
        resources
            .ingresses
            .sort_by_key(|i| (i.creation_timestamp(), i.namespace(), i.name_any()));
        let mut groups: BTreeMap<(std::net::SocketAddr, String), Vec<Arc<Route>>> = BTreeMap::new();
        let mut cert_names = BTreeSet::new();
        let mut uids = BTreeSet::new();
        let mut resolved = BTreeMap::new();
        let mut namespace_counts = BTreeMap::<String, (usize, usize, BTreeSet<String>)>::new();
        for ingress in &resources.ingresses {
            let Some(spec) = &ingress.spec else { continue };
            if !spec
                .ingress_class_name
                .as_ref()
                .map_or(default, |c| c == &options.class)
            {
                continue;
            }
            selected.push(ingress.clone());
            let ns = ingress.namespace().unwrap_or_default();
            let quota = tenant_policy.and_then(|p| p.quota(&ns));
            if tenant_policy.is_some() && quota.is_none() {
                diagnostics.push(Diagnostic {
                    ingress: ingress.clone(),
                    reason: "NamespaceDenied",
                    message: "namespace is not admitted by administrator policy".into(),
                });
                continue;
            }
            if let Some(quota) = quota {
                let resource_uid = ingress.uid().unwrap_or_default();
                let restored = {
                    let plugin_uid = ingress
                        .annotations()
                        .get("rgnix.io/script")
                        .and_then(|v| v.split_once('/'))
                        .and_then(|(name, _)| {
                            resources.maps.iter().find(|m| {
                                m.namespace().as_deref() == Some(&ns) && m.name_any() == name
                            })
                        })
                        .and_then(|m| m.uid())
                        .unwrap_or_else(|| "builtin".into());
                    checkpoint::restore(&resources, options, &resource_uid, &plugin_uid)
                };
                let retained = history
                    .routes
                    .get(&resource_uid)
                    .or_else(|| restored.as_ref().map(|a| &a.spec));
                let counts = namespace_counts.entry(ns.clone()).or_default();
                let references: Vec<_> = spec
                    .default_backend
                    .iter()
                    .chain(
                        spec.rules
                            .iter()
                            .flatten()
                            .flat_map(|r| r.http.iter())
                            .flat_map(|h| &h.paths)
                            .map(|p| &p.backend),
                    )
                    .collect();
                let retained_references: Vec<_> = retained
                    .into_iter()
                    .flat_map(|s| {
                        s.default_backend.iter().chain(
                            s.rules
                                .iter()
                                .flatten()
                                .flat_map(|r| r.http.iter())
                                .flat_map(|h| &h.paths)
                                .map(|p| &p.backend),
                        )
                    })
                    .collect();
                let route_count = references.len().max(retained_references.len());
                let mut backends = counts.2.clone();
                for service in retained_references
                    .iter()
                    .filter_map(|b| b.service.as_ref())
                {
                    backends.insert(backend_alias(service));
                }
                for service in references.iter().filter_map(|b| b.service.as_ref()) {
                    backends.insert(backend_alias(service));
                }
                let policy_values = policy::annotations(ingress.annotations());
                for policy_values in std::iter::once(&policy_values).chain(
                    history
                        .accepted
                        .get(&resource_uid)
                        .or(restored.as_ref())
                        .map(|a| &a.policy),
                ) {
                    if let Some(traffic) = policy_values
                        .get("rgnix.io/traffic-policy")
                        .and_then(|v| crate::rollout::Policy::parse(v).ok())
                    {
                        for service in traffic.services() {
                            backends.insert(service.into());
                        }
                    }
                    if let Some(auth) = policy_values
                        .get("rgnix.io/auth-service")
                        .and_then(|v| policy::auth_service(v).ok())
                    {
                        backends.insert(backend_alias(&auth.0));
                    }
                }
                let script = ingress
                    .annotations()
                    .get("rgnix.io/script")
                    .and_then(|v| v.split_once('/'))
                    .and_then(|(name, key)| {
                        resources
                            .maps
                            .iter()
                            .find(|m| m.namespace().as_deref() == Some(&ns) && m.name_any() == name)
                            .and_then(|m| m.data.as_ref()?.get(key))
                    });
                if counts.0 >= quota.max_ingresses
                    || counts.1 + route_count > quota.max_routes
                    || backends.len() > quota.max_backends
                    || (ingress.annotations().contains_key("rgnix.io/script")
                        && !quota.allow_scripts)
                    || script.is_some_and(|s| s.len() > quota.max_script_bytes)
                {
                    diagnostics.push(Diagnostic {
                        ingress: ingress.clone(),
                        reason: "NamespaceQuota",
                        message: "namespace ingress/route/backend/script ceiling exceeded".into(),
                    });
                    continue;
                }
                counts.0 += 1;
                counts.1 += route_count;
                counts.2 = backends;
            }
            let uid = ingress
                .uid()
                .unwrap_or_else(|| format!("{ns}/{}", ingress.name_any()));
            uids.insert(uid.clone());
            let mut invalid = false;
            let mut reuse_routes = false;
            let policy_values = policy::annotations(ingress.annotations());
            let route_policy = policy::parse(&policy_values).and_then(|(mut settings, _, _)| {
                if let Some(value) = policy_values.get("rgnix.io/traffic-policy") {
                    let traffic = crate::rollout::Policy::parse(value)?;
                    anyhow::ensure!(
                        traffic
                            .metric_gates
                            .iter()
                            .all(|name| control_policy.metrics.gates.contains_key(name)),
                        "traffic policy references an unconfigured metric gate"
                    );
                }

                policy::apply_resources(&mut settings, &policy_values, &resources, &ns)?;
                Ok(settings.body_policy)
            });
            let reference = ingress
                .annotations()
                .get("rgnix.io/script")
                .cloned()
                .or_else(|| {
                    (!policy_values.is_empty() || history.accepted.contains_key(&uid))
                        .then(String::new)
                });
            let script = if let Some(reference) = reference {
                let script_source = if reference.is_empty() {
                    Some((policy::PASS.to_owned(), "builtin".to_owned()))
                } else {
                    reference.split_once('/').and_then(|(name, key)| {
                        resources
                            .maps
                            .iter()
                            .find(|m| m.namespace().as_deref() == Some(&ns) && m.name_any() == name)
                            .and_then(|m| {
                                Some((
                                    m.data.as_ref()?.get(key)?.clone(),
                                    m.uid().unwrap_or_default(),
                                ))
                            })
                    })
                };
                if let Some((source, plugin_uid)) = script_source {
                    if history
                        .accepted
                        .get(&uid)
                        .is_some_and(|a| a.plugin_uid != plugin_uid)
                    {
                        history.scripts.remove(&uid);
                        history.routes.remove(&uid);
                        history.accepted.remove(&uid);
                        history.proposals.remove(&uid);
                        history.revoked.insert(uid.clone());
                    }
                    let durable = checkpoint::restore(&resources, options, &uid, &plugin_uid)
                        .filter(|a| a.namespace == ns && a.name == ingress.name_any());
                    if !history.revoked.contains(&uid)
                        && let Some(accepted) = durable.as_ref()
                        && history.accepted.get(&uid) != Some(accepted)
                        && let Ok(script) = shared
                            .compiler
                            .from_bytes(accepted.source.as_bytes(), false)
                    {
                        history.scripts.insert(uid.clone(), script);
                        history.routes.insert(uid.clone(), accepted.spec.clone());
                        history.accepted.insert(uid.clone(), accepted.clone());
                    }
                    match route_policy
                        .as_ref()
                        .map_err(|e| anyhow::anyhow!("{e:#}"))
                        .and_then(|_| shared.compiler.from_bytes(source.as_bytes(), false))
                    {
                        Ok(script) => {
                            let candidate = checkpoint::Accepted {
                                schema: 1,
                                class: options.class.clone(),
                                uid: uid.clone(),
                                namespace: ns.clone(),
                                name: ingress.name_any(),
                                reference: reference.clone(),
                                plugin_uid,
                                source: source.clone(),
                                spec: spec.clone(),
                                body_policy: *route_policy.as_ref().unwrap(),
                                policy: policy_values.clone(),
                            };
                            // Publication follows the checkpoint watch acknowledgement. A version
                            // that served traffic is therefore available to replacement replicas.
                            if preview || durable.as_ref() == Some(&candidate) {
                                history.scripts.insert(uid.clone(), script.clone());
                                history.accepted.insert(uid.clone(), candidate);
                                history.proposals.remove(&uid);
                                history.revoked.remove(&uid);
                                Some(script)
                            } else {
                                history.proposals.insert(uid.clone(), (candidate, script));
                                let previous = history.scripts.get(&uid).cloned();
                                reuse_routes = previous.is_some();
                                invalid = previous.is_none();
                                diagnostics.push(Diagnostic {
                                    ingress: ingress.clone(),
                                    reason: if invalid {
                                        "UnrecoverablePlugin"
                                    } else {
                                        "PendingCheckpoint"
                                    },
                                    message:
                                        "waiting for durable plugin checkpoint before publication"
                                            .into(),
                                });
                                previous
                            }
                        }
                        Err(e) => {
                            history.proposals.remove(&uid);
                            if !preview {
                                shared.telemetry.reload_errors.inc();
                            }
                            diagnostics.push(Diagnostic {
                                ingress: ingress.clone(),
                                reason: "InvalidPlugin",
                                message: format!("plugin rejected: {e:#}"),
                            });
                            let previous = history.scripts.get(&uid).cloned();
                            reuse_routes = previous.is_some();
                            if previous.is_none() {
                                invalid = true;
                                diagnostics.push(Diagnostic {
                                    ingress: ingress.clone(),
                                    reason: "UnrecoverablePlugin",
                                    message: "no accepted plugin version; route disabled".into(),
                                });
                            }
                            previous
                        }
                    }
                } else {
                    invalid = true;
                    history.scripts.remove(&uid);
                    history.accepted.remove(&uid);
                    history.proposals.remove(&uid);
                    history.revoked.insert(uid.clone());
                    diagnostics.push(Diagnostic {
                        ingress: ingress.clone(),
                        reason: "UnrecoverablePlugin",
                        message: "plugin withdrawn; route disabled".into(),
                    });
                    diagnostics.push(Diagnostic {
                        ingress: ingress.clone(),
                        reason: "MissingPlugin",
                        message: "plugin ConfigMap/key missing; routes disabled".into(),
                    });
                    None
                }
            } else {
                history.scripts.remove(&uid);
                history.accepted.remove(&uid);
                history.proposals.remove(&uid);
                history.revoked.insert(uid.clone());
                None
            };
            let script = script.filter(|_| {
                history
                    .accepted
                    .get(&uid)
                    .is_none_or(|a| !a.reference.is_empty())
            });
            let effective_policy = history
                .accepted
                .get(&uid)
                .map(|a| &a.policy)
                .unwrap_or(&policy_values);
            let (mut settings, backend_options, backend_tls) =
                policy::parse(effective_policy).unwrap_or_default();
            if let Some(accepted) = history.accepted.get(&uid) {
                settings.body_policy = accepted.body_policy;
            }
            if let Err(error) =
                policy::apply_resources(&mut settings, effective_policy, &resources, &ns)
            {
                invalid = true;
                diagnostics.push(Diagnostic {
                    ingress: ingress.clone(),
                    reason: "UnavailablePolicyResource",
                    message: format!("route disabled: {error:#}"),
                });
            }
            if let Some(quota) = quota {
                quota.constrain(&mut settings);
                if history.accepted.get(&uid).is_some_and(|a| {
                    a.source.len() > quota.max_script_bytes
                        || (!a.reference.is_empty() && !quota.allow_scripts)
                }) {
                    invalid = true;
                    diagnostics.push(Diagnostic {
                        ingress: ingress.clone(),
                        reason: "NamespaceQuota",
                        message: "retained plugin exceeds administrator ceiling".into(),
                    });
                }
            }
            let tenant = quota.map(|q| shared.tenants.tenant(&ns, q));
            settings.identity.trusted = options.identity_policy.trusted.clone();
            settings.identity.header = options.identity_policy.header.clone();
            settings.identity.recursive = options.identity_policy.recursive;
            settings.request_headers = vec![
                ("Host".into(), "$host".into()),
                ("X-Forwarded-For".into(), "$remote_addr".into()),
                ("X-Forwarded-Proto".into(), "$scheme".into()),
                ("X-Real-IP".into(), "$remote_addr".into()),
            ];
            let routing = if reuse_routes {
                history
                    .routes
                    .get(&uid)
                    .cloned()
                    .unwrap_or_else(|| spec.clone())
            } else {
                spec.clone()
            };
            let mut domain_spec = routing.clone();
            domain_spec.tls = spec.tls.clone();
            if let Err(error) = crate::tenancy::domains::claim(
                &ns,
                &domain_spec,
                tenant_policy.and_then(|p| p.domains.as_ref()),
                &mut domain_owners,
            ) {
                diagnostics.push(Diagnostic {
                    ingress: ingress.clone(),
                    reason: "DomainDenied",
                    message: error.to_string(),
                });
                continue;
            }
            if invalid {
                history.routes.remove(&uid);
            } else {
                history.routes.insert(uid.clone(), routing.clone());
            }
            let mut declared = vec![];
            if let Some(backend) = &routing.default_backend {
                if let Some(service) = &backend.service {
                    declared.push(service.clone())
                } else {
                    diagnostics.push(Diagnostic {
                        ingress: ingress.clone(),
                        reason: "UnsupportedBackend",
                        message: "resource backends are unsupported".into(),
                    });
                }
            }
            for rule in routing.rules.iter().flatten() {
                for path in rule.http.iter().flat_map(|h| &h.paths) {
                    if let Some(service) = &path.backend.service {
                        declared.push(service.clone());
                    }
                }
            }
            let traffic_policy = effective_policy
                .get("rgnix.io/traffic-policy")
                .map(|v| crate::rollout::Policy::parse(v))
                .transpose()?;
            if let Some(traffic) = &traffic_policy {
                if traffic.mirror.is_some() && quota.is_some_and(|q| !q.allow_mirroring) {
                    invalid = true;
                    diagnostics.push(Diagnostic {
                        ingress: ingress.clone(),
                        reason: "NamespaceQuota",
                        message: "administrator policy forbids mirroring".into(),
                    });
                }
                for service in traffic.services() {
                    declared.push(policy::auth_service(&format!("{service}/"))?.0);
                }
            }
            let mut allowed = BTreeMap::new();
            for service in &declared {
                let alias = backend_alias(service);
                let key = if effective_policy.is_empty() {
                    format!("{ns}/{alias}")
                } else {
                    format!("{ns}/{}/{}", ingress.name_any(), alias)
                };
                let (backend, warning) = resolved.entry(key.clone()).or_insert_with(|| {
                    let (mut backend, warning) = resolve_backend(&resources, &ns, service);
                    backend.tls = backend_tls;
                    backend.options = backend_options.clone();
                    if let Some(name) = &settings.upstream.server_name {
                        backend.hostname = name.clone();
                    }
                    backend.ca_pem = settings.upstream.ca_pem.clone();
                    backend.profile = settings.upstream.clone();
                    (Arc::new(backend), warning)
                });
                if let Some(message) = warning {
                    diagnostics.push(Diagnostic {
                        ingress: ingress.clone(),
                        reason: "UnavailableBackend",
                        message: message.clone(),
                    });
                }
                snapshot.backends.insert(key.clone(), backend.clone());
                allowed.insert(alias, key);
            }
            let rollout = traffic_policy.map(|policy| {
                shared.rollouts.get(
                    &format!("{ns}/{}", ingress.name_any()),
                    &uid,
                    policy,
                    allowed.clone(),
                )
            });
            if let Some(rollout) = &rollout
                && let Err(error) = rollout.sync_progress(
                    ingress
                        .annotations()
                        .get(release::PROGRESS)
                        .map(String::as_str),
                )
            {
                diagnostics.push(Diagnostic {
                    ingress: ingress.clone(),
                    reason: "InvalidRolloutProgress",
                    message: error.to_string(),
                });
            }
            if let Some(rollout) = &rollout
                && ingress
                    .annotations()
                    .get("rgnix.io/rolled-back-revision")
                    .is_some_and(|r| r == &rollout.policy.revision)
            {
                rollout.force_rollback();
                diagnostics.push(Diagnostic {
                    ingress: ingress.clone(),
                    reason: "TrafficRolledBack",
                    message: format!(
                        "traffic revision {} uses its fallback backend",
                        rollout.policy.revision
                    ),
                });
            }
            let mut route_specs = vec![];
            if let Some(backend) = &routing.default_backend {
                route_specs.push((
                    String::new(),
                    PathMatch::IngressDefault,
                    backend.service.as_ref(),
                ));
            }
            for rule in routing.rules.iter().flatten() {
                let host = rule.host.clone().unwrap_or_default().to_ascii_lowercase();
                for path in rule.http.iter().flat_map(|h| &h.paths) {
                    let value = path.path.clone().unwrap_or_else(|| "/".into());
                    let matcher = match path.path_type.as_str() {
                        "Exact" => PathMatch::Exact(value),
                        "Prefix" | "ImplementationSpecific" => PathMatch::IngressPrefix(
                            value.trim_end_matches('/').to_string()
                                + if value.trim_end_matches('/').is_empty() {
                                    "/"
                                } else {
                                    ""
                                },
                        ),
                        _ => {
                            diagnostics.push(Diagnostic {
                                ingress: ingress.clone(),
                                reason: "UnsupportedPath",
                                message: format!("unsupported path type {}", path.path_type),
                            });
                            continue;
                        }
                    };
                    route_specs.push((host.clone(), matcher, path.backend.service.as_ref()));
                }
            }
            for (host, matcher, service) in route_specs {
                let action = if invalid {
                    Action::Unavailable
                } else {
                    service
                        .and_then(|s| allowed.get(&backend_alias(s)))
                        .map(|key| Action::Proxy {
                            backend: key.clone(),
                            uri: None,
                        })
                        .unwrap_or(Action::Unavailable)
                };
                let route = Arc::new(Route {
                    id: format!(
                        "{ns}/{}:{}:{}",
                        ingress.name_any(),
                        host,
                        if matches!(matcher, PathMatch::IngressDefault) {
                            "default"
                        } else {
                            matcher.path()
                        }
                    ),
                    tenant: tenant.clone(),
                    rollout: rollout.clone(),
                    matcher: matcher.clone(),
                    action,
                    settings: settings.clone(),
                    script: if invalid { None } else { script.clone() },
                    allowed_backends: allowed.clone(),
                });
                for listener in &listeners {
                    let routes = groups.entry((listener.address, host.clone())).or_default();
                    if routes.iter().any(|r| r.matcher == matcher) {
                        diagnostics.push(Diagnostic {
                            ingress: ingress.clone(),
                            reason: "RouteConflict",
                            message: if matches!(matcher, PathMatch::IngressDefault) {
                                "default backend is owned by an earlier Ingress".into()
                            } else {
                                format!(
                                    "route {host}{} is owned by an earlier Ingress",
                                    matcher.path()
                                )
                            },
                        });
                    } else {
                        routes.push(route.clone());
                    }
                }
            }
            for tls in spec.tls.iter().flatten() {
                let claims: Vec<_> = tls
                    .hosts
                    .iter()
                    .flatten()
                    .flat_map(|name| {
                        listeners
                            .iter()
                            .filter(|l| l.tls)
                            .map(move |l| (l.address, name.to_ascii_lowercase()))
                    })
                    .filter(|(listener, name)| {
                        if cert_names.insert((*listener, name.clone())) {
                            true
                        } else {
                            diagnostics.push(Diagnostic {
                                ingress: ingress.clone(),
                                reason: "TLSConflict",
                                message: format!(
                                    "TLS host {name} is owned by an earlier Ingress declaration"
                                ),
                            });
                            false
                        }
                    })
                    .collect();
                if claims.is_empty() {
                    continue;
                }
                let cert = tls
                    .secret_name
                    .as_ref()
                    .and_then(|secret_name| {
                        resources.secrets.iter().find(|s| {
                            s.namespace().as_deref() == Some(&ns) && s.name_any() == *secret_name
                        })
                    })
                    .and_then(|s| s.data.as_ref())
                    .and_then(|data| Some((&data.get("tls.crt")?.0, &data.get("tls.key")?.0)))
                    .context("TLS Secret or key missing")
                    .and_then(|(cert, key)| {
                        let certificate = Certificate::parse(cert, key)?;
                        for (_, name) in &claims {
                            certificate.validate_name(name)?;
                        }
                        Ok(certificate)
                    });
                let certificate = match cert {
                    Ok(cert) => Some(Arc::new(cert)),
                    Err(e) => {
                        diagnostics.push(Diagnostic {
                            ingress: ingress.clone(),
                            reason: "InvalidTLS",
                            message: format!("TLS host disabled: {e:#}"),
                        });
                        None
                    }
                };
                for (listener, name) in claims {
                    snapshot.certificates.push(TlsHost {
                        listener,
                        name,
                        ingress: true,
                        default: false,
                        certificate: certificate.clone(),
                        client_auth: settings.security.mtls.clone(),
                    });
                }
            }
        }
        shared
            .tenants
            .retain(&namespace_counts.keys().cloned().collect());
        history.scripts.retain(|uid, _| uids.contains(uid));
        history.routes.retain(|uid, _| uids.contains(uid));
        history.accepted.retain(|uid, _| uids.contains(uid));
        history.proposals.retain(|uid, _| uids.contains(uid));
        history.revoked.retain(|uid| uids.contains(uid));
        snapshot.hosts = groups
            .into_iter()
            .map(|((listener, name), routes)| VirtualHost {
                listener,
                names: vec![name.clone()],
                default: name.is_empty(),
                ingress: true,
                routes,
            })
            .collect();
        Ok((snapshot, diagnostics, selected))
    })();
    (result, history)
}

fn backend_alias(service: &IngressServiceBackend) -> String {
    let port = service
        .port
        .as_ref()
        .and_then(|p| p.name.clone().or_else(|| p.number.map(|n| n.to_string())))
        .unwrap_or_default();
    format!("{}:{port}", service.name)
}
fn resolve_backend(
    resources: &Resources,
    namespace: &str,
    reference: &IngressServiceBackend,
) -> (Backend, Option<String>) {
    let mut endpoints = vec![];
    let result = (|| -> Result<()> {
        let service = resources
            .services
            .iter()
            .find(|s| s.namespace().as_deref() == Some(namespace) && s.name_any() == reference.name)
            .context("Service missing")?;
        let spec = service.spec.as_ref().context("Service spec missing")?;
        ensure!(
            spec.type_.as_deref() != Some("ExternalName"),
            "ExternalName Services are unsupported"
        );
        let port_ref = reference.port.as_ref().context("Service port missing")?;
        let port = spec
            .ports
            .as_ref()
            .context("Service has no ports")?
            .iter()
            .find(|p| {
                port_ref.number == Some(p.port)
                    || port_ref
                        .name
                        .as_ref()
                        .is_some_and(|name| p.name.as_ref() == Some(name))
            })
            .context("Service port not found")?;
        ensure!(
            port.protocol.as_deref().unwrap_or("TCP") == "TCP",
            "only TCP Services are supported"
        );
        let mut unique = BTreeSet::new();
        for slice in &resources.slices {
            if slice.namespace().as_deref() != Some(namespace)
                || slice.labels().get("kubernetes.io/service-name") != Some(&reference.name)
                || !["IPv4", "IPv6"].contains(&slice.address_type.as_str())
                || slice.owner_references().iter().any(|owner| {
                    owner.api_version == "v1"
                        && owner.kind == "Service"
                        && (owner.name != reference.name
                            || service.metadata.uid.as_deref() != Some(owner.uid.as_str()))
                })
            {
                continue;
            }
            for endpoint_port in slice.ports.iter().flatten().filter(|p| {
                p.name.as_deref().unwrap_or("") == port.name.as_deref().unwrap_or("")
                    && p.protocol.as_deref().unwrap_or("TCP") == "TCP"
            }) {
                let Some(number) = endpoint_port
                    .port
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|n| *n > 0)
                else {
                    continue;
                };
                for endpoint in &slice.endpoints {
                    if endpoint
                        .conditions
                        .as_ref()
                        .is_some_and(|c| c.ready == Some(false) || c.terminating == Some(true))
                    {
                        continue;
                    }
                    for address in &endpoint.addresses {
                        if let Ok(ip) = address.parse::<std::net::IpAddr>() {
                            unique.insert(std::net::SocketAddr::new(ip, number));
                        }
                    }
                }
            }
        }
        endpoints = unique
            .into_iter()
            .map(|address| Endpoint { address, weight: 1 })
            .collect();
        Ok(())
    })();
    let hostname = format!("{}.{namespace}.svc", reference.name);
    (
        Backend::new(endpoints, false, hostname.clone(), hostname),
        result
            .err()
            .map(|e| format!("{}: {e:#}", backend_alias(reference))),
    )
}
