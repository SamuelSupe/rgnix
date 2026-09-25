use anyhow::{Result, ensure};
use k8s_openapi::api::networking::v1::IngressSpec;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Domains(pub BTreeMap<String, BTreeSet<String>>);

pub fn namespace_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn valid_host(host: &str) -> bool {
    let name = host.strip_prefix("*.").unwrap_or(host);
    host.is_empty() || (name.len() <= 253 && name.split('.').all(namespace_name))
}
fn covers(pattern: &str, host: &str) -> bool {
    pattern == host
        || (!host.starts_with("*.")
            && pattern.strip_prefix("*.").is_some_and(|suffix| {
                host.strip_suffix(suffix)
                    .and_then(|p| p.strip_suffix('.'))
                    .is_some_and(|p| !p.is_empty() && !p.contains('.'))
            }))
}
fn overlap(left: &str, right: &str) -> bool {
    covers(left, right) || covers(right, left)
}

impl Domains {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.0.len() <= 4096, "at most 4096 domain grants");
        for (host, namespaces) in &self.0 {
            ensure!(
                valid_host(host)
                    && !namespaces.is_empty()
                    && namespaces.iter().all(|n| namespace_name(n)),
                "invalid domain grant"
            );
            for (other, owners) in self.0.range(..host.clone()) {
                ensure!(
                    !overlap(host, other) || namespaces == owners,
                    "overlapping domain grants must authorize the same namespaces"
                );
            }
        }
        Ok(())
    }
    pub fn authorize(&self, namespace: &str, spec: &IngressSpec) -> Result<()> {
        for host in hosts(spec) {
            ensure!(
                self.0
                    .iter()
                    .any(|(grant, owners)| covers(grant, &host) && owners.contains(namespace)),
                "namespace {namespace} is not authorized for domain {host:?}"
            );
        }
        Ok(())
    }
}
pub fn hosts(spec: &IngressSpec) -> BTreeSet<String> {
    let mut names: BTreeSet<_> = spec
        .rules
        .iter()
        .flatten()
        .map(|r| r.host.clone().unwrap_or_default().to_ascii_lowercase())
        .chain(
            spec.tls
                .iter()
                .flatten()
                .flat_map(|t| t.hosts.iter().flatten())
                .map(|n| n.to_ascii_lowercase()),
        )
        .collect();
    if spec.default_backend.is_some() {
        names.insert(String::new());
    }
    names
}

/// Without explicit grants, domains belong to the first namespace, across all path types.
pub fn claim(
    namespace: &str,
    spec: &IngressSpec,
    grants: Option<&Domains>,
    owners: &mut BTreeMap<String, String>,
) -> Result<()> {
    if let Some(grants) = grants {
        return grants.authorize(namespace, spec);
    }
    let names = hosts(spec);
    for host in &names {
        ensure!(
            !owners
                .iter()
                .any(|(other, owner)| owner != namespace && overlap(host, other)),
            "domain {host:?} belongs to another namespace; configure an explicit shared domain grant"
        );
    }
    for host in names {
        owners.entry(host).or_insert_with(|| namespace.into());
    }
    Ok(())
}
