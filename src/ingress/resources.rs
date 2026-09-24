use super::{CONTROLLER, History, Options, Resources};
use k8s_openapi::api::networking::v1::IngressSpec;
use kube::ResourceExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

impl Resources {
    pub(super) fn prepare(&mut self, options: &Options, history: &History) -> [u8; 32] {
        self.classes.retain(|c| c.name_any() == options.class);
        let class = self.classes.first();
        let enabled = class.is_some_and(|c| {
            c.spec
                .as_ref()
                .is_some_and(|s| s.controller.as_deref() == Some(CONTROLLER))
        });
        let default = class.is_some_and(|c| {
            c.annotations()
                .get("ingressclass.kubernetes.io/is-default-class")
                .is_some_and(|v| v == "true")
        });
        self.ingresses.retain(|i| {
            enabled
                && i.spec.as_ref().is_some_and(|s| {
                    s.ingress_class_name
                        .as_ref()
                        .map_or(default, |c| c == &options.class)
                })
        });
        self.ingresses
            .sort_by_key(|i| (i.creation_timestamp(), i.namespace(), i.name_any()));
        let mut services = BTreeSet::new();
        let mut secrets = BTreeSet::new();
        let mut maps = BTreeSet::new();
        let mut checkpoints = BTreeSet::new();
        for ingress in &self.ingresses {
            let ns = ingress.namespace().unwrap_or_default();
            let uid = ingress.uid().unwrap_or_default();
            for spec in ingress.spec.iter().chain(history.routes.get(&uid)) {
                service_dependencies(spec, &ns, &mut services);
            }
            if let Some(spec) = &ingress.spec {
                for tls in spec.tls.iter().flatten() {
                    if let Some(name) = &tls.secret_name {
                        secrets.insert((ns.clone(), name.clone()));
                    }
                }
            }
            if let Some((name, _)) = ingress
                .annotations()
                .get("rgnix.io/script")
                .and_then(|r| r.split_once('/'))
            {
                maps.insert((ns, name.to_string()));
                {
                    let plugin_uid = self
                        .maps
                        .iter()
                        .find(|m| m.namespace() == ingress.namespace() && m.name_any() == name)
                        .and_then(|m| m.uid());
                    if let Some(accepted) = plugin_uid
                        .as_deref()
                        .and_then(|id| super::checkpoint::restore(self, options, &uid, id))
                        && accepted.namespace == ingress.namespace().unwrap_or_default()
                        && accepted.name == ingress.name_any()
                    {
                        service_dependencies(&accepted.spec, &accepted.namespace, &mut services);
                    }
                    checkpoints.insert(super::checkpoint::name(&options.class, &uid));
                }
            }
        }
        self.services
            .retain(|s| services.contains(&(s.namespace().unwrap_or_default(), s.name_any())));
        self.slices.retain(|s| {
            s.labels()
                .get("kubernetes.io/service-name")
                .is_some_and(|name| {
                    services.contains(&(s.namespace().unwrap_or_default(), name.clone()))
                })
        });
        self.secrets
            .retain(|s| secrets.contains(&(s.namespace().unwrap_or_default(), s.name_any())));
        self.maps.retain(|m| {
            maps.contains(&(m.namespace().unwrap_or_default(), m.name_any()))
                || (m.namespace().as_deref() == Some(&options.publish_namespace)
                    && checkpoints.contains(&m.name_any()))
        });
        self.services.sort_by_key(|s| (s.namespace(), s.name_any()));
        self.slices.sort_by_key(|s| (s.namespace(), s.name_any()));
        self.secrets.sort_by_key(|s| (s.namespace(), s.name_any()));
        self.maps.sort_by_key(|s| (s.namespace(), s.name_any()));
        // Status, resourceVersion and unrelated resources must not trigger recompilation/publication.
        let value = json!({
            "class": self.classes.iter().map(|c| json!([c.uid(), c.spec, c.annotations().get("ingressclass.kubernetes.io/is-default-class")])).collect::<Vec<_>>(),
            "ingresses": self.ingresses.iter().map(|i| json!([i.uid(), i.namespace(), i.name_any(), i.creation_timestamp(), i.spec, i.annotations().get("rgnix.io/script")])).collect::<Vec<_>>(),
            "services": self.services.iter().map(|s| json!([s.namespace(), s.name_any(), s.uid(), s.spec])).collect::<Vec<_>>(),
            "slices": self.slices.iter().map(|s| json!([s.namespace(), s.name_any(), s.address_type, s.endpoints, s.ports, s.labels().get("kubernetes.io/service-name"), s.owner_references()])).collect::<Vec<_>>(),
            "secrets": self.secrets.iter().map(|s| json!([s.namespace(), s.name_any(), s.data])).collect::<Vec<_>>(),
            "maps": self.maps.iter().map(|m| json!([m.namespace(), m.name_any(), m.uid(), m.data])).collect::<Vec<_>>(),
        });
        Sha256::digest(value.to_string().as_bytes()).into()
    }
}

fn service_dependencies(spec: &IngressSpec, ns: &str, names: &mut BTreeSet<(String, String)>) {
    let backends = spec.default_backend.iter().chain(
        spec.rules
            .iter()
            .flatten()
            .flat_map(|r| r.http.iter())
            .flat_map(|h| &h.paths)
            .map(|p| &p.backend),
    );
    for service in backends.filter_map(|b| b.service.as_ref()) {
        names.insert((ns.into(), service.name.clone()));
    }
}
