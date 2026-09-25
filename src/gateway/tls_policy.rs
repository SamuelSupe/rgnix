use super::{
    CONTROLLER, GROUP,
    controller::{Options, Resources},
    references::{find, objects, text},
    status::{Update, condition},
};
use crate::upstream::Transport;
use anyhow::{Context, Result, ensure};
use kube::ResourceExt;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

type Target = (String, String, Option<String>);
pub(super) struct Policies(BTreeMap<Target, std::result::Result<Transport, String>>);
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Spec {
    target_refs: Vec<super::spec::Reference>,
    validation: Validation,
    #[serde(default)]
    options: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Validation {
    hostname: String,
    #[serde(default)]
    ca_certificate_refs: Vec<super::spec::Reference>,
    well_known_ca_certificates: Option<String>,
}
fn transport(resources: &Resources, namespace: &str, spec: &Spec) -> Result<Transport> {
    ensure!(
        spec.target_refs.len() == 1 && spec.options.is_empty(),
        "one targetRef and no implementation-specific TLS options are supported"
    );
    let target = &spec.target_refs[0];
    ensure!(
        target.group.as_deref().unwrap_or("").is_empty()
            && target.kind.as_deref() == Some("Service")
            && target.namespace.is_none()
            && target.port.is_none(),
        "targetRef must be a local Service, optionally with sectionName"
    );
    ensure!(
        !spec.validation.hostname.starts_with('*')
            && super::routes::valid_hostname(&spec.validation.hostname),
        "invalid backend TLS hostname"
    );
    let mut transport = Transport::default();
    transport.server_name = Some(spec.validation.hostname.clone());
    if spec.validation.ca_certificate_refs.is_empty() {
        ensure!(
            spec.validation.well_known_ca_certificates.as_deref() == Some("System"),
            "InvalidCACertificateRef: system trust or a CA reference is required"
        );
    } else {
        ensure!(
            spec.validation.well_known_ca_certificates.is_none()
                && spec.validation.ca_certificate_refs.len() <= 8,
            "InvalidCACertificateRef: choose system trust or at most eight CA references"
        );
        let mut pem = String::new();
        for reference in &spec.validation.ca_certificate_refs {
            ensure!(
                reference.group.as_deref().unwrap_or("").is_empty()
                    && reference.namespace.is_none()
                    && reference.section_name.is_none()
                    && reference.port.is_none(),
                "InvalidCACertificateRef: CA references must be local objects"
            );
            let kind = reference.kind.as_deref().unwrap_or("ConfigMap");
            let object = find(resources, kind, namespace, &reference.name)
                .context("InvalidCACertificateRef: CA object is missing")?;
            let ca = match kind {
                "ConfigMap" => object.data["data"]["ca.crt"]
                    .as_str()
                    .context("InvalidCACertificateRef: ca.crt missing")?
                    .to_owned(),
                "Secret" => {
                    let secret: k8s_openapi::api::core::v1::Secret =
                        serde_json::from_value(serde_json::to_value(object)?)?;
                    String::from_utf8(
                        secret
                            .data
                            .context("InvalidCACertificateRef: Secret data missing")?
                            .get("ca.crt")
                            .context("InvalidCACertificateRef: ca.crt missing")?
                            .0
                            .clone(),
                    )?
                }
                _ => anyhow::bail!("InvalidCACertificateRef: unsupported CA kind"),
            };
            ensure!(
                pem.len() + ca.len() < 1024 * 1024,
                "InvalidCACertificateRef: CA bundle exceeds 1 MiB"
            );
            pem.push_str(&ca);
            pem.push('\n');
        }
        transport
            .set_ca(pem)
            .context("InvalidCACertificateRef: invalid CA bundle")?;
    }
    Ok(transport)
}
impl Policies {
    pub(super) fn build(
        resources: &Resources,
        options: &Options,
        updates: &mut Vec<Update>,
    ) -> Self {
        let mut policies: Vec<_> = objects(resources, "BackendTLSPolicy").iter().collect();
        policies.sort_by_key(|p| (p.creation_timestamp(), p.namespace(), p.name_any()));
        let mut targets = BTreeMap::new();
        for object in policies {
            let namespace = object.namespace().unwrap_or_default();
            let spec = serde_json::from_value::<Spec>(object.data["spec"].clone());
            let result = spec
                .as_ref()
                .map_err(|e| anyhow::anyhow!("unsupported BackendTLSPolicy: {e}"))
                .and_then(|s| transport(resources, &namespace, s))
                .map_err(|e| format!("{e:#}"));
            // Even an invalid TLS policy claims its target: traffic must fail closed.
            let mut conflicted = false;
            let mut found = false;
            for target in object.data["spec"]["targetRefs"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if text(target, "group", "").is_empty() && text(target, "kind", "") == "Service" {
                    let name = text(target, "name", "");
                    let section = target["sectionName"].as_str().map(str::to_owned);
                    found |= find(resources, "Service", &namespace, name).is_some_and(|service| {
                        section.as_ref().is_none_or(|section| {
                            service.data["spec"]["ports"]
                                .as_array()
                                .is_some_and(|ports| {
                                    ports.iter().any(|port| port["name"] == *section)
                                })
                        })
                    });
                    let key = (namespace.clone(), name.to_owned(), section);
                    if let std::collections::btree_map::Entry::Vacant(entry) = targets.entry(key) {
                        entry.insert(result.clone());
                    } else {
                        conflicted = true;
                    }
                }
            }
            let accepted = result.is_ok() && found && !conflicted;
            let ca_error = result
                .as_ref()
                .err()
                .is_some_and(|e| e.contains("InvalidCACertificateRef"));
            let reason = if conflicted {
                "Conflicted"
            } else if !found {
                "TargetNotFound"
            } else if ca_error {
                "NoValidCACertificate"
            } else if accepted {
                "Accepted"
            } else {
                "Invalid"
            };
            let message = result
                .as_ref()
                .err()
                .map(String::as_str)
                .unwrap_or(if conflicted {
                    "An older policy owns this target"
                } else {
                    "Backend TLS policy evaluated"
                });
            updates.push(Update::new("BackendTLSPolicy", object, json!({"ancestors":[{
                "ancestorRef":{"group":GROUP,"kind":"Gateway","namespace":options.namespace,"name":options.name},
                "controllerName":CONTROLLER,"conditions":[condition(object,"Accepted",accepted,reason,message),
                    condition(object,"ResolvedRefs",!ca_error,if ca_error {"InvalidCACertificateRef"} else {"ResolvedRefs"},message)]
            }]})));
        }
        Self(targets)
    }
    pub(super) fn apply(
        &self,
        namespace: &str,
        service: &str,
        section: Option<&str>,
        backend: &mut crate::backend::Backend,
    ) -> Result<bool> {
        let policy = section
            .and_then(|s| {
                self.0
                    .get(&(namespace.into(), service.into(), Some(s.into())))
            })
            .or_else(|| self.0.get(&(namespace.into(), service.into(), None)));
        let Some(policy) = policy else {
            return Ok(false);
        };
        let mut transport = policy.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?.clone();
        transport.protocol = backend.profile.protocol.clone();
        transport.identity = backend.profile.identity.clone();
        transport.identity_pem = backend.profile.identity_pem.clone();
        transport.identity_digest = backend.profile.identity_digest.clone();
        backend.tls = true;
        backend.hostname = transport.server_name.clone().unwrap();
        backend.ca_pem = transport.ca_pem.clone();
        backend.profile = transport;
        Ok(true)
    }
}
