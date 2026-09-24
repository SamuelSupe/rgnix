mod checkpoint;
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
    pub publish_namespace: String,
    pub publish_service: String,
    pub identity: String,
}
struct Stores {
    ingresses: Store<Ingress>,
    classes: Store<IngressClass>,
    services: Store<Service>,
    slices: Store<EndpointSlice>,
    secrets: Store<Secret>,
    maps: Store<ConfigMap>,
}
struct Resources {
    ingresses: Vec<Arc<Ingress>>,
    classes: Vec<Arc<IngressClass>>,
    services: Vec<Arc<Service>>,
    slices: Vec<Arc<EndpointSlice>>,
    secrets: Vec<Arc<Secret>>,
    maps: Vec<Arc<ConfigMap>>,
}
#[derive(Default)]
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
    client: Client,
    changed: watch::Sender<()>,
    mut shutdown: pingora::server::ShutdownWatch,
) -> Store<K>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Send + Sync + 'static,
{
    let (store, writer) = reflector::store::<K>();
    tokio::spawn(async move {
        let events = watcher(Api::<K>::all(client), watcher::Config::default());
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
impl Stores {
    async fn ready(&self) -> Result<()> {
        tokio::try_join!(
            self.ingresses.wait_until_ready(),
            self.classes.wait_until_ready(),
            self.services.wait_until_ready(),
            self.slices.wait_until_ready(),
            self.secrets.wait_until_ready(),
            self.maps.wait_until_ready()
        )?;
        Ok(())
    }
    fn snapshot(&self) -> Resources {
        Resources {
            ingresses: self.ingresses.state(),
            classes: self.classes.state(),
            services: self.services.state(),
            slices: self.slices.state(),
            secrets: self.secrets.state(),
            maps: self.maps.state(),
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
    let stores = Stores {
        ingresses: observe(client.clone(), tx.clone(), shutdown.clone()),
        classes: observe(client.clone(), tx.clone(), shutdown.clone()),
        services: observe(client.clone(), tx.clone(), shutdown.clone()),
        slices: observe(client.clone(), tx.clone(), shutdown.clone()),
        secrets: observe(client.clone(), tx.clone(), shutdown.clone()),
        maps: observe(client.clone(), tx, shutdown.clone()),
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
    tokio::spawn(async move {
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
    loop {
        let mut resources = stores.snapshot();
        let input = resources.prepare(&options, &history);
        if previous_input == Some(input) {
            tokio::select! { _=shutdown.changed()=>return Ok(()), _=changed.changed()=>{} }
            continue;
        }
        previous_input = Some(input);
        let options_copy = options.clone();
        let state = shared.clone();
        let result =
            tokio::task::spawn_blocking(move || build(&state, &options_copy, resources, history))
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
        }
    }
}

type Built = Result<(RuntimeSnapshot, Vec<Diagnostic>, Vec<Arc<Ingress>>)>;
fn build(
    shared: &Shared,
    options: &Options,
    mut resources: Resources,
    mut history: History,
) -> (Built, History) {
    let result = (|| -> Built {
        let listeners = shared.snapshot.load().listeners.clone();
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
            let uid = ingress
                .uid()
                .unwrap_or_else(|| format!("{ns}/{}", ingress.name_any()));
            uids.insert(uid.clone());
            let mut invalid = false;
            let mut reuse_routes = false;
            let script = if let Some(reference) = ingress.annotations().get("rgnix.io/script") {
                let script_source = reference.split_once('/').and_then(|(name, key)| {
                    resources
                        .maps
                        .iter()
                        .find(|m| m.namespace().as_deref() == Some(&ns) && m.name_any() == name)
                        .and_then(|m| {
                            Some((m.data.as_ref()?.get(key)?, m.uid().unwrap_or_default()))
                        })
                });
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
                    match shared.compiler.from_bytes(source.as_bytes(), false) {
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
                            };
                            // Publication follows the checkpoint watch acknowledgement. A version
                            // that served traffic is therefore available to replacement replicas.
                            if durable.as_ref() == Some(&candidate) {
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
                            shared.telemetry.reload_errors.inc();
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
            let routing = if reuse_routes {
                history
                    .routes
                    .get(&uid)
                    .cloned()
                    .unwrap_or_else(|| spec.clone())
            } else {
                spec.clone()
            };
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
            let mut allowed = BTreeMap::new();
            for service in &declared {
                let alias = backend_alias(service);
                let key = format!("{ns}/{alias}");
                let (backend, warning) = resolved.entry(key.clone()).or_insert_with(|| {
                    let (backend, warning) = resolve_backend(&resources, &ns, service);
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
            let settings = Settings {
                request_headers: vec![
                    ("Host".into(), "$host".into()),
                    ("X-Forwarded-For".into(), "$remote_addr".into()),
                    ("X-Forwarded-Proto".into(), "$scheme".into()),
                    ("X-Real-IP".into(), "$remote_addr".into()),
                ],
                ..Settings::default()
            };
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
                    .and_then(|(cert, key)| Certificate::parse(cert, key));
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
                    });
                }
            }
        }
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
