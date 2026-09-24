use crate::model::{PathMatch, Route, VirtualHost};
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

#[derive(Default)]
pub(crate) struct Index {
    listeners: BTreeMap<SocketAddr, Hosts>,
    paths: Vec<Paths>,
}
#[derive(Default)]
struct Hosts {
    exact: BTreeMap<String, usize>,
    wildcard: BTreeMap<String, usize>,
    default: Option<usize>,
}
#[derive(Default)]
struct Paths {
    exact: BTreeMap<String, Arc<Route>>,
    nginx: BTreeMap<String, Arc<Route>>,
    ingress: BTreeMap<String, Arc<Route>>,
    fallback: Option<Arc<Route>>,
}

impl Index {
    pub(crate) fn build(hosts: &[VirtualHost]) -> Self {
        let mut index = Self::default();
        for (id, host) in hosts.iter().enumerate() {
            let names = index.listeners.entry(host.listener).or_default();
            if host.default && names.default.is_none() {
                names.default = Some(id);
            }
            for name in &host.names {
                if name.is_empty() && host.ingress {
                    names.default.get_or_insert(id);
                } else if let Some(suffix) = name.strip_prefix("*.") {
                    names.wildcard.entry(suffix.into()).or_insert(id);
                } else {
                    // NGINX keeps the first virtual host for a conflicting server_name.
                    names.exact.entry(name.clone()).or_insert(id);
                }
            }
            let mut paths = Paths::default();
            for route in &host.routes {
                match &route.matcher {
                    PathMatch::Exact(path) => {
                        paths
                            .exact
                            .entry(path.clone())
                            .or_insert_with(|| route.clone());
                    }
                    PathMatch::NginxPrefix(path) => {
                        paths
                            .nginx
                            .entry(path.clone())
                            .or_insert_with(|| route.clone());
                    }
                    PathMatch::IngressPrefix(path) => {
                        paths
                            .ingress
                            .entry(path.trim_end_matches('/').into())
                            .or_insert_with(|| route.clone());
                    }
                    PathMatch::IngressDefault => {
                        paths.fallback.get_or_insert_with(|| route.clone());
                    }
                }
            }
            index.paths.push(paths);
        }
        index
    }

    pub(crate) fn hostless_server_name<'a>(
        &self,
        hosts: &'a [VirtualHost],
        listener: SocketAddr,
    ) -> Option<&'a str> {
        let names = self.listeners.get(&listener)?;
        let selected = names.exact.get("").copied().or(names.default)?;
        hosts[selected].names.first().map(String::as_str)
    }

    pub(crate) fn route(
        &self,
        hosts: &[VirtualHost],
        listener: SocketAddr,
        host: &str,
        path: &str,
    ) -> Option<Arc<Route>> {
        let names = self.listeners.get(&listener)?;
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let selected = names
            .exact
            .get(&host)
            .copied()
            .or_else(|| {
                host.match_indices('.').find_map(|(dot, _)| {
                    let id = *names.wildcard.get(&host[dot + 1..])?;
                    (dot > 0 && (!hosts[id].ingress || !host[..dot].contains('.'))).then_some(id)
                })
            })
            .or(names.default)?;
        self.paths[selected].route(path).or_else(|| {
            if hosts[selected].ingress {
                names.default.and_then(|id| self.paths[id].route(path))
            } else {
                None
            }
        })
    }
}

impl Paths {
    fn route(&self, path: &str) -> Option<Arc<Route>> {
        if let Some(route) = self.exact.get(path) {
            return Some(route.clone());
        }
        // Bound lookup by URI length rather than the number of configured routes.
        for end in std::iter::once(path.len()).chain(path.char_indices().rev().map(|(i, _)| i)) {
            let prefix = &path[..end];
            if let Some(route) = self.nginx.get(prefix) {
                return Some(route.clone());
            }
            if (end == path.len() || path[end..].starts_with('/'))
                && let Some(route) = self.ingress.get(prefix)
            {
                return Some(route.clone());
            }
        }
        self.fallback.clone()
    }
}
