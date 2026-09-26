use super::references::{allows, certificate, find, objects, permitted, text};
use super::{
    CONTROLLER, GROUP,
    controller::{Options, Resources},
    routes::{self, Entry, ListenerRoutes, Routing},
    spec::{Reference, RouteSpec},
    status::{Update, condition},
};
use crate::{model::*, runtime::Shared, script::CompiledScript};
use anyhow::{Context, Result, ensure};
use k8s_openapi::api::{
    core::v1::Service,
    discovery::v1::EndpointSlice,
    networking::v1::{IngressServiceBackend, ServiceBackendPort},
};
use kube::{ResourceExt, core::DynamicObject};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Default)]
pub(super) struct History {
    scripts: BTreeMap<String, GoodScript>,
    pending_restores: BTreeSet<String>,
    pub(super) strict: Option<String>,
}
#[derive(Clone)]
struct GoodScript {
    reference: String,
    configmap_uid: String,
    spec: RouteSpec,
    script: Arc<CompiledScript>,
    source_bytes: usize,
    annotations: BTreeMap<String, String>,
    accepted: super::checkpoint::Accepted,
}
impl History {
    pub(super) fn checkpoints(&self) -> BTreeMap<String, super::checkpoint::Accepted> {
        self.scripts
            .iter()
            .map(|(uid, script)| (uid.clone(), script.accepted.clone()))
            .collect()
    }
}
pub(super) struct Built {
    pub snapshot: RuntimeSnapshot,
    pub updates: Vec<Update>,
}

fn conditions(
    object: &DynamicObject,
    accepted: bool,
    reason: &str,
    message: &str,
    resolved: bool,
    ref_reason: &str,
) -> Vec<Value> {
    vec![
        condition(object, "Accepted", accepted, reason, message),
        condition(
            object,
            "ResolvedRefs",
            resolved,
            ref_reason,
            if resolved {
                "References resolved"
            } else {
                "A backend or certificate reference cannot be used"
            },
        ),
    ]
}

fn script(
    resources: &Resources,
    object: &DynamicObject,
    mut spec: RouteSpec,
    shared: &Shared,
    history: &mut History,
    quota: Option<&crate::tenancy::Quota>,
    owner: (&Options, &str),
) -> Result<(RouteSpec, Option<Arc<CompiledScript>>, bool)> {
    let uid = object.uid().unwrap_or_default();
    let Some(reference) = object.annotations().get("rgnix.io/script") else {
        history.scripts.remove(&uid);
        return Ok((spec, None, false));
    };
    ensure!(
        quota.is_none_or(|q| q.allow_scripts),
        "namespace scripts are disabled"
    );
    let (name, key) = reference
        .split_once('/')
        .context("script must be configmap/key")?;
    let map = find(
        resources,
        "ConfigMap",
        &object.namespace().unwrap_or_default(),
        name,
    )
    .context("plugin ConfigMap missing")?;
    let source = map.data["data"][key]
        .as_str()
        .context("plugin key missing")?;
    ensure!(
        source.len() <= quota.map_or(256 * 1024, |q| q.max_script_bytes),
        "script exceeds namespace size limit"
    );
    let map_uid = map.uid().unwrap_or_default();
    if history.strict.as_ref() != Some(&uid)
        && !history.scripts.contains_key(&uid)
        && let Some(accepted) = super::checkpoint::restore(resources, owner.0, object, owner.1)
        && accepted.reference == *reference
        && accepted.configmap_uid == map_uid
        && accepted.source.len() <= quota.map_or(256 * 1024, |q| q.max_script_bytes)
    {
        match shared.compiler.for_tenant(
            &object.namespace().unwrap_or_default(),
            accepted.source.as_bytes(),
            quota.map_or(60, |q| q.max_compilations_per_minute),
            history.strict.is_some(),
        ) {
            Ok(script) => {
                history.scripts.insert(
                    uid.clone(),
                    GoodScript {
                        reference: accepted.reference.clone(),
                        configmap_uid: accepted.configmap_uid.clone(),
                        spec: accepted.spec.clone(),
                        script,
                        source_bytes: accepted.source.len(),
                        annotations: accepted.annotations.clone(),
                        accepted,
                    },
                );
            }
            Err(error) if error.is::<crate::script::PendingCompilation>() => {
                history.pending_restores.insert(uid.clone());
            }
            Err(_) => {}
        }
    }
    match shared.compiler.for_tenant(
        &object.namespace().unwrap_or_default(),
        source.as_bytes(),
        quota.map_or(60, |q| q.max_compilations_per_minute),
        history.strict.is_some(),
    ) {
        Ok(script) => {
            history.pending_restores.remove(&uid);
            let accepted = super::checkpoint::Accepted {
                schema: 1,
                gateway_uid: find(resources, "Gateway", &owner.0.namespace, &owner.0.name)
                    .and_then(|g| g.uid())
                    .unwrap_or_default(),
                namespace: object.namespace().unwrap_or_default(),
                name: object.name_any(),
                kind: owner.1.into(),
                uid: uid.clone(),
                reference: reference.clone(),
                configmap_uid: map_uid.clone(),
                source: source.into(),
                spec: spec.clone(),
                annotations: super::checkpoint::annotations(object),
            };
            ensure!(
                serde_json::to_vec(&accepted)?.len() <= 900 * 1024,
                "Gateway checkpoint exceeds 900 KiB"
            );
            history.scripts.insert(
                uid,
                GoodScript {
                    reference: reference.clone(),
                    configmap_uid: map_uid,
                    spec: spec.clone(),
                    script: script.clone(),
                    source_bytes: source.len(),
                    annotations: object.annotations().clone(),
                    accepted,
                },
            );
            Ok((spec, Some(script), false))
        }
        Err(error) => {
            ensure!(
                history.strict.as_ref() != Some(&uid),
                "plugin compilation failed: {error:#}"
            );
            let previous = history
                .scripts
                .get(&uid)
                .filter(|s| s.reference == *reference && s.configmap_uid == map_uid)
                .with_context(|| format!("plugin compilation failed: {error:#}"))?;
            ensure!(
                quota.is_none_or(|q| previous.source_bytes <= q.max_script_bytes),
                "retained script exceeds the current namespace quota"
            );
            spec = previous.spec.clone();
            Ok((spec, Some(previous.script.clone()), true))
        }
    }
}

fn settings(
    object: &DynamicObject,
    options: &Options,
    quota: Option<&crate::tenancy::Quota>,
    grpc: bool,
    resources: &crate::ingress::Resources,
) -> Result<(Settings, crate::backend::Options, bool)> {
    let annotations = crate::ingress::policy::annotations(object.annotations());
    ensure!(
        !annotations.contains_key("rgnix.io/client-ca-secret")
            && !annotations.contains_key("rgnix.io/verify-client"),
        "frontend client certificate policy is not supported on Gateway routes"
    );
    let (mut settings, backend, tls) = crate::ingress::policy::parse(&annotations)?;
    let access = settings.identity.access.clone();
    settings.identity = options.identity_policy.clone();
    settings.identity.access = access;
    crate::ingress::policy::apply_resources(
        &mut settings,
        &annotations,
        resources,
        &object.namespace().unwrap_or_default(),
    )?;
    if grpc {
        settings.upstream.protocol = crate::upstream::Protocol::Http2;
    }
    if let Some(quota) = quota {
        quota.constrain(&mut settings);
    }
    Ok((settings, backend, tls))
}

pub(super) fn parent_targets(parent: &Reference, route_ns: &str, options: &Options) -> bool {
    parent.group.as_deref().unwrap_or(GROUP) == GROUP
        && parent.kind.as_deref().unwrap_or("Gateway") == "Gateway"
        && parent.namespace.as_deref().unwrap_or(route_ns) == options.namespace
        && parent.name == options.name
}

pub(super) fn build(
    resources: &Resources,
    options: &Options,
    shared: &Shared,
    mut history: History,
) -> (Built, History) {
    history.pending_restores.clear();
    let mut snapshot = RuntimeSnapshot::empty(shared.snapshot.load().listeners.clone());
    snapshot.ready = false;
    snapshot.gateway = Some(Routing::default());
    let mut updates = vec![];
    let Some(gateway) = find(resources, "Gateway", &options.namespace, &options.name) else {
        history.scripts.clear();
        return (Built { snapshot, updates }, history);
    };
    let class_name = text(&gateway.data["spec"], "gatewayClassName", "");
    let class = find(resources, "GatewayClass", "", class_name);
    let owned = class.is_some_and(|c| text(&c.data["spec"], "controllerName", "") == CONTROLLER);
    if !owned {
        history.scripts.clear();
        return (Built { snapshot, updates }, history);
    }
    let class = class.unwrap();
    let class_valid = class.data["spec"].get("parametersRef").is_none();
    updates.push(Update::new("GatewayClass", class, json!({"conditions":[condition(class, "Accepted", class_valid, if class_valid {"Accepted"} else {"InvalidParameters"}, "Pre-provisioned rgnix Gateway; parametersRef is unsupported")]})));
    let spec = &gateway.data["spec"];
    let gateway_valid = class_valid
        && spec.as_object().is_some_and(|s| {
            s.keys()
                .all(|k| ["gatewayClassName", "listeners"].contains(&k.as_str()))
        });
    let listeners = spec["listeners"].as_array().cloned().unwrap_or_default();
    let mut listener_statuses = vec![];
    let mut active = vec![];
    let mut seen = BTreeSet::new();
    for (index, listener) in listeners.iter().enumerate() {
        let name = text(listener, "name", "");
        let hostname = text(listener, "hostname", "");
        let protocol = text(listener, "protocol", "");
        let port = listener["port"].as_u64().unwrap_or(0);
        let address = snapshot
            .listeners
            .iter()
            .find(|l| {
                (protocol == "HTTP" && !l.tls && port == u64::from(options.http_port))
                    || (protocol == "HTTPS" && l.tls && port == u64::from(options.https_port))
            })
            .map(|l| l.address);
        let mut valid = gateway_valid
            && address.is_some()
            && (hostname.is_empty() || routes::valid_hostname(hostname));
        valid &= listener.as_object().is_some_and(|o| {
            o.keys().all(|k| {
                [
                    "name",
                    "hostname",
                    "port",
                    "protocol",
                    "tls",
                    "allowedRoutes",
                ]
                .contains(&k.as_str())
            })
        });
        valid &= seen.insert((port, hostname));
        let mut reason = if valid {
            "Accepted"
        } else {
            "UnsupportedValue"
        };
        let mut message = if valid {
            "Listener accepted".into()
        } else {
            "Listener requires a unique hostname, HTTP/HTTPS and a configured external port; unsupported fields are rejected".into()
        };
        let mut resolved = true;
        let mut ref_reason = "ResolvedRefs";
        if valid
            && protocol == "HTTPS"
            && let Err(error) = certificate(
                resources,
                gateway,
                listener,
                &mut snapshot,
                address.unwrap(),
            )
        {
            resolved = false;
            valid = false;
            reason = "Invalid";
            message = format!("{error:#}");
            ref_reason = if message.contains("RefNotPermitted") {
                "RefNotPermitted"
            } else {
                "InvalidCertificateRef"
            };
        }
        if protocol == "HTTP" && listener.get("tls").is_some() {
            valid = false;
            reason = "UnsupportedValue";
            message = "HTTP listener cannot configure TLS".into();
        }
        let mut conditions = conditions(gateway, valid, reason, &message, resolved, ref_reason);
        conditions.push(condition(
            gateway,
            "Programmed",
            valid,
            if valid { "Programmed" } else { "Invalid" },
            &message,
        ));
        listener_statuses.push(json!({"name":name,"supportedKinds":[{"group":GROUP,"kind":"HTTPRoute"},{"group":GROUP,"kind":"GRPCRoute"}],"attachedRoutes":0,"conditions":conditions}));
        // Retain the hostname even when its TLS configuration is invalid, preventing fallback to a broader listener.
        if let Some(address) = address {
            let routing = snapshot.gateway.as_mut().unwrap();
            routing.listeners.push(ListenerRoutes {
                address,
                hostname: hostname.into(),
                entries: vec![],
            });
            active.push((index, routing.listeners.len() - 1, valid));
        }
    }
    let services: Vec<Arc<Service>> = objects(resources, "Service")
        .iter()
        .filter_map(|s| {
            serde_json::from_value(serde_json::to_value(s).ok()?)
                .ok()
                .map(Arc::new)
        })
        .collect();
    let slices: Vec<Arc<EndpointSlice>> = objects(resources, "EndpointSlice")
        .iter()
        .filter_map(|s| {
            serde_json::from_value(serde_json::to_value(s).ok()?)
                .ok()
                .map(Arc::new)
        })
        .collect();
    let dependencies = crate::ingress::Resources {
        ingresses: vec![],
        classes: vec![],
        services: services.clone(),
        slices: slices.clone(),
        secrets: objects(resources, "Secret")
            .iter()
            .filter_map(|s| {
                serde_json::from_value(serde_json::to_value(s).ok()?)
                    .ok()
                    .map(Arc::new)
            })
            .collect(),
        maps: vec![],
    };
    let tls_policies = super::tls_policy::Policies::build(resources, options, &mut updates);
    let controls = shared.controls.active.load_full();
    let tenants = controls.tenant_policy.as_ref();
    let mut counts = BTreeMap::<String, (usize, usize, BTreeSet<String>)>::new();
    let mut domains = BTreeMap::new();
    let mut live_scripts = BTreeSet::new();
    let mut route_objects: Vec<_> = ["HTTPRoute", "GRPCRoute"]
        .into_iter()
        .flat_map(|kind| objects(resources, kind).iter().map(move |o| (kind, o)))
        .collect();
    route_objects.sort_by_key(|(_, o)| (o.creation_timestamp(), o.namespace(), o.name_any()));
    for (kind, object) in route_objects {
        let ns = object.namespace().unwrap_or_default();
        let parsed = serde_json::from_value::<RouteSpec>(object.data["spec"].clone());
        let raw_parents: Vec<Reference> = parsed
            .as_ref()
            .map(|s| s.parent_refs.clone())
            .unwrap_or_else(|_| {
                serde_json::from_value(object.data["spec"]["parentRefs"].clone())
                    .unwrap_or_default()
            });
        let parents: Vec<_> = raw_parents
            .into_iter()
            .filter(|p| parent_targets(p, &ns, options))
            .collect();
        if parents.is_empty() {
            if object.data["status"]["parents"]
                .as_array()
                .is_some_and(|parents| {
                    parents.iter().any(|p| {
                        p["controllerName"] == CONTROLLER
                            && super::status::owns_parent(&p["parentRef"], object, options)
                    })
                })
            {
                updates.push(Update::new(kind, object, json!({"parents":[]})));
            }
            continue;
        }
        let uid = object.uid().unwrap_or_default();
        live_scripts.insert(uid.clone());
        let previous_script = history.scripts.get(&uid).cloned();
        let quota = tenants.and_then(|p| p.quota(&ns));
        let mut stale = false;
        let mut rejected_spec = None;
        let prepared = (|| -> Result<_> {
            ensure!(
                options.namespaces.is_empty() || options.namespaces.contains(&ns),
                "namespace is outside watch scope"
            );
            ensure!(
                tenants.is_none() || quota.is_some(),
                "namespace denied by administrator policy"
            );
            let spec = parsed?;
            ensure!(
                spec.rules.len() <= 64 && spec.hostnames.len() <= 16,
                "route specification exceeds limits"
            );
            ensure!(
                spec.hostnames.iter().all(|h| routes::valid_hostname(h)),
                "invalid route hostname"
            );
            let (spec, script, retained) = script(
                resources,
                object,
                spec,
                shared,
                &mut history,
                quota,
                (options, kind),
            )?;
            stale = retained;
            let ingress_spec = k8s_openapi::api::networking::v1::IngressSpec {
                rules: Some(if spec.hostnames.is_empty() {
                    vec![k8s_openapi::api::networking::v1::IngressRule {
                        host: None,
                        http: None,
                    }]
                } else {
                    spec.hostnames
                        .iter()
                        .map(|h| k8s_openapi::api::networking::v1::IngressRule {
                            host: Some(h.clone()),
                            http: None,
                        })
                        .collect()
                }),
                ..Default::default()
            };
            // The administrator's explicit domain grants also govern Gateway routes.
            if let Some(grants) = tenants.and_then(|p| p.domains.as_ref()) {
                grants.authorize(&ns, &ingress_spec)?;
            }
            let count = counts.entry(ns.clone()).or_default();
            let backends: BTreeSet<_> = spec
                .rules
                .iter()
                .flat_map(|r| r.all_backends())
                .map(|b| {
                    format!(
                        "{}/{}:{}",
                        b.namespace.as_deref().unwrap_or(&ns),
                        b.name,
                        b.port.unwrap_or(0)
                    )
                })
                .collect();
            let matches = spec
                .rules
                .iter()
                .map(|r| r.matches.len().max(1))
                .sum::<usize>()
                * spec.hostnames.len().max(1)
                * active.len().max(1);
            ensure!(
                quota.is_none_or(|q| count.0 < q.max_ingresses
                    && count.1 + matches <= q.max_routes
                    && count.2.union(&backends).count() <= q.max_backends),
                "namespace configuration quota exceeded"
            );
            count.0 += 1;
            count.1 += matches;
            count.2.extend(backends);
            let mut accepted_object = (**object).clone();
            if retained && let Some(previous) = history.scripts.get(&uid) {
                accepted_object.metadata.annotations = Some(previous.annotations.clone());
            }
            for rule in &spec.rules {
                ensure!(
                    kind != "GRPCRoute" || rule.timeouts.is_none(),
                    "GRPCRoute timeouts are not supported"
                );
                ensure!(
                    rule.mirror().is_none() || quota.is_none_or(|q| q.allow_mirroring),
                    "administrator policy forbids mirroring"
                );
                ensure!(
                    rule.name
                        .as_deref()
                        .is_none_or(crate::tenancy::namespace_name),
                    "invalid rule name"
                );
                for matcher in rule
                    .matches
                    .iter()
                    .cloned()
                    .chain(if rule.matches.is_empty() {
                        Some(Default::default())
                    } else {
                        None
                    })
                {
                    let path = matcher.path_match(kind == "GRPCRoute")?;
                    routes::filters(rule, 80, matches!(path, PathMatch::IngressPrefix(_)))?;
                }
            }
            // An unavailable auth dependency must not expose a broader unauthenticated route.
            rejected_spec = Some(spec.clone());
            let settings = settings(
                &accepted_object,
                options,
                quota,
                kind == "GRPCRoute",
                &dependencies,
            )?;
            let traffic = accepted_object
                .annotations()
                .get("rgnix.io/traffic-policy")
                .map(|v| crate::rollout::Policy::parse(v))
                .transpose()?;
            if let Some(traffic) = &traffic {
                ensure!(
                    traffic.mirror.is_none() || spec.rules.iter().all(|r| r.mirror().is_none()),
                    "standard RequestMirror and traffic-policy mirror cannot be combined"
                );
                ensure!(
                    traffic.mirror.is_none() || quota.is_none_or(|q| q.allow_mirroring),
                    "administrator policy forbids mirroring"
                );
                for rule in &spec.rules {
                    ensure!(
                        traffic
                            .services()
                            .all(|alias| rule.backend_refs.iter().any(|b| b
                                .namespace
                                .as_deref()
                                .unwrap_or(&ns)
                                == ns
                                && b.group.as_deref().unwrap_or("").is_empty()
                                && b.kind.as_deref().unwrap_or("Service") == "Service"
                                && alias == format!("{}:{}", b.name, b.port.unwrap_or(0)))),
                        "traffic policy targets must be numeric-port Service backendRefs in every rule in this namespace"
                    );
                    ensure!(
                        !rule.filters.iter().any(|f| f.type_ == "RequestRedirect"),
                        "traffic policy cannot be combined with RequestRedirect"
                    );
                }
            }
            Ok((spec, script, settings, traffic))
        })();
        if prepared.is_err() {
            let previous_script = previous_script.filter(|previous| {
                object.annotations().get("rgnix.io/script") == Some(&previous.reference)
                    && previous
                        .reference
                        .split_once('/')
                        .is_some_and(|(name, key)| {
                            find(resources, "ConfigMap", &ns, name).is_some_and(|map| {
                                map.uid().as_deref() == Some(&previous.configmap_uid)
                                    && map.data["data"][key].is_string()
                            })
                        })
            });
            if let Some(previous) = previous_script {
                history.scripts.insert(uid.clone(), previous);
            } else {
                history.scripts.remove(&uid);
            }
        }
        let rejected = if prepared.is_err() {
            rejected_spec.map(|spec| {
                (
                    spec,
                    None,
                    (
                        Settings::default(),
                        crate::backend::Options::default(),
                        false,
                    ),
                    None,
                )
            })
        } else {
            None
        };
        let failed_policy = rejected.is_some();
        let plan = prepared.as_ref().ok().or(rejected.as_ref());
        let mut parent_statuses = vec![];
        for parent in parents {
            let mut accepted = false;
            let mut resolved = true;
            let mut ref_reason = "ResolvedRefs";
            let mut reason = "NoMatchingParent";
            let mut message = "No matching listener".to_owned();
            if let Some((spec, script, (settings, backend_options, backend_tls), traffic)) = plan {
                for (listener_index, route_index, valid) in &active {
                    let listener = &listeners[*listener_index];
                    if parent
                        .section_name
                        .as_ref()
                        .is_some_and(|s| s != text(listener, "name", ""))
                        || parent
                            .port
                            .is_some_and(|p| Some(u64::from(p)) != listener["port"].as_u64())
                    {
                        continue;
                    }
                    if !valid || !allows(resources, listener, &options.namespace, &ns, kind) {
                        reason = "NotAllowedByListeners";
                        message = "Listener is invalid or does not allow this route".into();
                        continue;
                    }
                    let route_hosts = if spec.hostnames.is_empty() {
                        vec![String::new()]
                    } else {
                        spec.hostnames.clone()
                    };
                    let hosts: Vec<_> = route_hosts
                        .iter()
                        .filter_map(|h| routes::intersect(text(listener, "hostname", ""), h))
                        .collect();
                    if hosts.is_empty() {
                        reason = "NoMatchingListenerHostname";
                        message = "Route and listener hostnames do not intersect".into();
                        continue;
                    }
                    accepted = !failed_policy;
                    reason = if failed_policy {
                        "UnsupportedValue"
                    } else {
                        "Accepted"
                    };
                    message = if stale {
                        "Previous valid plugin and route retained after compilation failure"
                    } else {
                        "Route accepted"
                    }
                    .into();
                    if failed_policy {
                        resolved = false;
                        ref_reason = "InvalidPolicy";
                        message = format!("{:#}", prepared.as_ref().err().unwrap());
                    }
                    for (rule_index, rule) in spec.rules.iter().enumerate() {
                        let mut weights = vec![];
                        let mut bindings = BTreeMap::new();
                        let mut mirror_backend = None;
                        for (backend, mirrored) in rule
                            .backend_refs
                            .iter()
                            .map(|b| (b, false))
                            .chain(rule.mirror().map(|m| (&m.backend_ref, true)))
                            .filter(|_| !failed_policy)
                        {
                            let weight = backend.weight.unwrap_or(1);
                            if weight == 0 && traffic.is_none() && !mirrored {
                                continue;
                            }
                            let backend_ns = backend.namespace.as_deref().unwrap_or(&ns);
                            let key = format!(
                                "gateway:{uid}:{backend_ns}/{}:{}",
                                backend.name,
                                backend.port.unwrap_or(0)
                            );
                            let invalid = if !backend.group.as_deref().unwrap_or("").is_empty()
                                || backend.kind.as_deref().unwrap_or("Service") != "Service"
                            {
                                Some("InvalidKind")
                            } else if !permitted(
                                resources,
                                &ns,
                                kind,
                                backend_ns,
                                "Service",
                                &backend.name,
                            ) {
                                Some("RefNotPermitted")
                            } else if !backend.filters.is_empty() || backend.port.is_none() {
                                Some("UnsupportedValue")
                            } else {
                                None
                            };
                            if let Some(error) = invalid {
                                resolved = false;
                                ref_reason = error;
                                if !mirrored {
                                    weights.push((None, weight));
                                }
                                continue;
                            }
                            let reference = IngressServiceBackend {
                                name: backend.name.clone(),
                                port: Some(ServiceBackendPort {
                                    name: None,
                                    number: backend.port.map(i32::from),
                                }),
                            };
                            let (mut backend_value, error) = crate::ingress::service_backend(
                                &services, &slices, backend_ns, &reference,
                            );
                            if error.is_some() {
                                resolved = false;
                                ref_reason = "BackendNotFound";
                                if !mirrored {
                                    weights.push((None, weight));
                                }
                                continue;
                            }
                            backend_value.tls = *backend_tls;
                            backend_value.options = backend_options.clone();
                            backend_value.profile = settings.upstream.clone();
                            backend_value.ca_pem = settings.upstream.ca_pem.clone();
                            if let Some(name) = &settings.upstream.server_name {
                                backend_value.hostname = name.clone();
                            }
                            let service_port = services
                                .iter()
                                .find(|s| {
                                    s.namespace().as_deref() == Some(backend_ns)
                                        && s.name_any() == backend.name
                                })
                                .and_then(|s| s.spec.as_ref())
                                .and_then(|s| s.ports.as_ref())
                                .and_then(|ports| {
                                    ports
                                        .iter()
                                        .find(|p| Some(p.port) == backend.port.map(i32::from))
                                });
                            let tls = tls_policies.apply(
                                backend_ns,
                                &backend.name,
                                service_port.and_then(|p| p.name.as_deref()),
                                &mut backend_value,
                            );
                            if tls.is_err() {
                                resolved = false;
                                ref_reason = "InvalidCACertificateRef";
                                if !mirrored {
                                    weights.push((None, weight));
                                }
                                continue;
                            }
                            match service_port.and_then(|p| p.app_protocol.as_deref()) {
                                None | Some("http") | Some("kubernetes.io/ws") => {}
                                Some("https") if backend_value.tls => {}
                                Some("kubernetes.io/h2c") => {
                                    backend_value.profile.protocol =
                                        crate::upstream::Protocol::Http2
                                }
                                Some(_) => {
                                    resolved = false;
                                    ref_reason = "UnsupportedProtocol";
                                    if !mirrored {
                                        weights.push((None, weight));
                                    }
                                    continue;
                                }
                            }
                            snapshot
                                .backends
                                .entry(key.clone())
                                .or_insert_with(|| Arc::new(backend_value));
                            if mirrored {
                                mirror_backend = Some(key);
                                continue;
                            }
                            bindings.insert(
                                format!("{backend_ns}/{}:{}", backend.name, backend.port.unwrap()),
                                key.clone(),
                            );
                            if backend_ns == ns {
                                bindings.insert(
                                    format!("{}:{}", backend.name, backend.port.unwrap()),
                                    key.clone(),
                                );
                            }
                            weights.push((Some(key), weight));
                        }
                        let rollout = traffic.as_ref().and_then(|policy| {
                            if !policy.services().all(|s| bindings.contains_key(s)) {
                                return None;
                            }
                            let state = shared.rollouts.get_for(
                                kind,
                                &format!("{ns}/{}", object.name_any()),
                                &uid,
                                policy.clone(),
                                bindings.clone(),
                            );
                            if state
                                .sync_progress(
                                    object
                                        .annotations()
                                        .get(crate::ingress::release::PROGRESS)
                                        .map(String::as_str),
                                )
                                .is_err()
                            {
                                return None;
                            }
                            if object.annotations().get("rgnix.io/rolled-back-revision")
                                == Some(&policy.revision)
                            {
                                state.force_rollback();
                            }
                            Some(state)
                        });
                        let invalid_traffic = traffic.is_some() && rollout.is_none();
                        if invalid_traffic {
                            resolved = false;
                            ref_reason = "BackendNotFound";
                            weights = vec![(None, 1)];
                            bindings.clear();
                        }
                        for (match_index, matcher) in rule
                            .matches
                            .iter()
                            .cloned()
                            .chain(if rule.matches.is_empty() {
                                Some(Default::default())
                            } else {
                                None
                            })
                            .enumerate()
                        {
                            let path = matcher.path_match(kind == "GRPCRoute").unwrap();
                            let mut policy = routes::filters(
                                rule,
                                listener["port"].as_u64().unwrap() as u16,
                                matches!(path, PathMatch::IngressPrefix(_)),
                            )
                            .unwrap();
                            if failed_policy {
                                policy = Default::default();
                            }
                            policy.backends = weights.clone();
                            if let Some(mirror) = &mut policy.mirror {
                                mirror.backend = mirror_backend.clone();
                            }
                            if let Some(quota) = quota {
                                let ceiling =
                                    std::time::Duration::from_secs(quota.max_timeout_seconds);
                                policy.timeouts.request =
                                    policy.timeouts.request.map(|v| v.min(ceiling));
                                policy.timeouts.backend_request =
                                    policy.timeouts.backend_request.map(|v| v.min(ceiling));
                            }
                            let mut settings = settings.clone();
                            settings.gateway = Some(policy);
                            let id = format!(
                                "{ns}/{}:gateway:{kind}:{listener_index}:{rule_index}:{match_index}",
                                object.name_any()
                            );
                            let route = Arc::new(Route {
                                id: id.clone(),
                                tenant: quota.map(|q| shared.tenants.tenant(&ns, q)),
                                rollout: rollout.clone(),
                                matcher: path,
                                action: Action::Unavailable,
                                settings,
                                script: if invalid_traffic {
                                    None
                                } else {
                                    script.clone()
                                },
                                allowed_backends: bindings.clone(),
                            });
                            for host in &hosts {
                                let routing = snapshot.gateway.as_mut().unwrap();
                                routing.listeners[*route_index].entries.push(Entry {
                                    hostname: host.clone(),
                                    matcher: matcher.clone(),
                                    grpc: kind == "GRPCRoute",
                                    order: (
                                        object
                                            .creation_timestamp()
                                            .map(|t| t.0.to_rfc3339())
                                            .unwrap_or_default(),
                                        ns.clone(),
                                        object.name_any(),
                                        rule_index,
                                        match_index,
                                    ),
                                    route_id: id.clone(),
                                    route: route.clone(),
                                });
                                snapshot.hosts.push(VirtualHost {
                                    listener: routing.listeners[*route_index].address,
                                    names: vec![host.clone()],
                                    default: host.is_empty(),
                                    ingress: false,
                                    routes: vec![route.clone()],
                                });
                            }
                        }
                    }
                    domains
                        .entry(*listener_index)
                        .or_insert_with(BTreeSet::new)
                        .insert((kind, ns.clone(), object.name_any()));
                }
            } else if let Err(error) = &prepared {
                reason = "UnsupportedValue";
                message = format!("{error:#}");
            }
            let mut route_conditions =
                conditions(object, accepted, reason, &message, resolved, ref_reason);
            if stale {
                route_conditions.push(condition(object, "rgnix.io/PluginReady", false, "CompilationFailed", "Previous valid plugin retained; current references and endpoint withdrawals still apply"));
            }
            parent_statuses.push(json!({"parentRef":parent,"controllerName":CONTROLLER,"conditions":route_conditions}));
        }
        updates.push(Update::new(
            kind,
            object,
            json!({"parents": parent_statuses}),
        ));
    }
    history.scripts.retain(|uid, _| live_scripts.contains(uid));
    shared.tenants.retain(&counts.into_keys().collect());
    for (index, status) in listener_statuses.iter_mut().enumerate() {
        status["attachedRoutes"] = json!(domains.get(&index).map_or(0, BTreeSet::len));
    }
    let listeners_ready = active.iter().any(|(_, _, valid)| *valid);
    snapshot.ready =
        listeners_ready && (shared.snapshot.load().ready || history.pending_restores.is_empty());
    let programmed = snapshot.ready;
    updates.push(Update::new("Gateway", gateway, json!({"listeners":listener_statuses,"conditions":[condition(gateway,"Accepted",gateway_valid,if gateway_valid {"Accepted"} else {"UnsupportedValue"},"Pre-provisioned Gateway uses the configured HTTP and HTTPS ports"),condition(gateway,"Programmed",programmed,if programmed {"Programmed"} else if listeners_ready {"Pending"} else {"Invalid"},if programmed {"Configuration published"} else if listeners_ready {"Waiting for accepted plugins to restore"} else {"No valid listener"})]})));
    (Built { snapshot, updates }, history)
}
