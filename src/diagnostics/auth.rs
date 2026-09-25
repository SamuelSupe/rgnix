use super::Options;
use crate::model::RuntimeSnapshot;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Reader,
    Writer,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub name: String,
    pub role: Role,
    /// An omitted scope grants global administration; an empty list grants nothing.
    pub namespaces: Option<BTreeSet<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    name: String,
    role: Role,
    namespaces: Option<BTreeSet<String>>,
    token_sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    users: Vec<User>,
}
#[derive(Default)]
pub struct Credentials {
    entries: Vec<([u8; 32], Principal)>,
}
impl Credentials {
    pub fn load(options: &Options) -> Result<Self> {
        let mut entries = vec![];
        for (path, name, role) in [
            (&options.admin_token_file, "writer", Role::Writer),
            (&options.admin_read_token_file, "reader", Role::Reader),
        ] {
            if let Some(hash) = Options::token_hash(path)? {
                entries.push((
                    hash,
                    Principal {
                        name: name.into(),
                        role,
                        namespaces: None,
                    },
                ));
            }
        }
        if let Some(path) = &options.admin_users_file {
            let bytes = crate::controls::read_bounded(path, 1024 * 1024)?;
            let file: File = serde_json::from_slice(&bytes)?;
            ensure!(
                file.users.len() <= 1024,
                "at most 1024 management identities"
            );
            for user in file.users {
                ensure!(
                    !user.name.is_empty()
                        && user.name.len() <= 128
                        && user
                            .name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b)),
                    "invalid management identity name"
                );
                ensure!(
                    user.token_sha256.len() == 64
                        && user.token_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                    "token_sha256 requires 64 hexadecimal digits"
                );
                let mut hash = [0; 32];
                for (i, byte) in hash.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&user.token_sha256[i * 2..i * 2 + 2], 16)
                        .map_err(|_| anyhow::anyhow!("invalid token_sha256"))?;
                }
                if let Some(names) = &user.namespaces {
                    ensure!(
                        names.len() <= 1024
                            && names.iter().all(|n| crate::tenancy::namespace_name(n)),
                        "invalid management namespace scope"
                    );
                }
                entries.push((
                    hash,
                    Principal {
                        name: user.name,
                        role: user.role,
                        namespaces: user.namespaces,
                    },
                ));
            }
        }
        let mut names = BTreeSet::new();
        let mut hashes = BTreeSet::new();
        for (hash, user) in &entries {
            ensure!(
                names.insert(&user.name) && hashes.insert(*hash),
                "management identities and tokens must be distinct"
            );
        }
        Ok(Self { entries })
    }
    pub fn authenticate(&self, headers: &http::HeaderMap) -> Option<Principal> {
        let mut values = headers.get_all("authorization").iter();
        let token = values.next()?.to_str().ok()?.strip_prefix("Bearer ")?;
        if token.len() > 4096 || values.next().is_some() {
            return None;
        }
        let hash = Sha256::digest(token.as_bytes());
        self.entries
            .iter()
            .find(|(expected, _)| openssl::memcmp::eq(expected, &hash))
            .map(|(_, user)| user.clone())
    }
}
impl Principal {
    pub fn writer(&self) -> bool {
        matches!(self.role, Role::Writer)
    }
    pub fn global(&self) -> bool {
        self.namespaces.is_none()
    }
    pub fn allows(&self, namespace: &str) -> bool {
        self.namespaces
            .as_ref()
            .is_none_or(|names| names.contains(namespace))
    }
    pub fn route(&self, route: &crate::model::Route) -> bool {
        self.global()
            || route
                .id
                .split_once('/')
                .is_some_and(|(ns, _)| self.allows(ns))
    }
    pub fn view(&self, snapshot: &RuntimeSnapshot) -> RuntimeSnapshot {
        let mut view = snapshot.clone();
        if self.global() {
            return view;
        }
        view.source_bundle = None;
        for host in &mut view.hosts {
            host.routes
                .retain(|route| host.ingress && self.route(route));
        }
        view.hosts.retain(|h| !h.routes.is_empty());
        let backends: BTreeSet<_> = view
            .hosts
            .iter()
            .flat_map(|h| &h.routes)
            .flat_map(|r| r.allowed_backends.values().cloned())
            .collect();
        view.backends.retain(|key, _| backends.contains(key));
        view.certificates.retain(|c| {
            view.hosts
                .iter()
                .any(|h| h.listener == c.listener && h.names.contains(&c.name))
        });
        view.reindex();
        view
    }
}
