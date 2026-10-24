use crate::model::{PathMatch, Route, VirtualHost};
use std::{borrow::Cow, collections::BTreeMap, net::SocketAddr, sync::Arc};

#[derive(Clone, Default)]
pub(crate) struct Index {
    listeners: BTreeMap<SocketAddr, Hosts>,
    paths: Vec<Paths>,
}
#[derive(Clone, Default)]
struct Hosts {
    exact: BTreeMap<String, usize>,
    wildcard: BTreeMap<String, usize>,
    default: Option<usize>,
}
#[derive(Clone, Default)]
struct Paths {
    exact: BTreeMap<String, Arc<Route>>,
    nginx: BTreeMap<String, Arc<Route>>,
    ingress: BTreeMap<String, Arc<Route>>,
    fallback: Option<Arc<Route>>,
    prefix_lengths: Vec<usize>,
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
            paths.prefix_lengths = paths
                .nginx
                .keys()
                .chain(paths.ingress.keys())
                .map(String::len)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .rev()
                .collect();
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
        let host = host.trim_end_matches('.');
        let host = if host.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(host.to_ascii_lowercase())
        } else {
            Cow::Borrowed(host)
        };
        let selected = names
            .exact
            .get(host.as_ref())
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
        // Use configured lengths for long URIs, but retain the URI-length bound
        // when a configuration contains many distinct prefix lengths.
        let configured = self.prefix_lengths.len() <= path.len();
        let lengths = self
            .prefix_lengths
            .iter()
            .copied()
            .take(if configured { usize::MAX } else { 0 })
            .filter(|end| *end <= path.len() && path.is_char_boundary(*end));
        let characters = std::iter::once(path.len())
            .chain(path.char_indices().rev().map(|(i, _)| i))
            .take(if configured { 0 } else { usize::MAX });
        let ends = lengths.chain(characters);
        for end in ends {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_prefix_lengths_preserve_longest_match_and_segment_boundaries() {
        let route = |id: &str, matcher| {
            Arc::new(Route {
                id: id.into(),
                metrics: Default::default(),
                tenant: None,
                rollout: None,
                matcher,
                action: crate::model::Action::Return {
                    status: 200,
                    text: id.into(),
                },
                settings: Default::default(),
                script: None,
                allowed_backends: Default::default(),
            })
        };
        let listener = "127.0.0.1:8080".parse().unwrap();
        let hosts = vec![VirtualHost {
            listener,
            names: vec!["test".into()],
            ingress: false,
            default: true,
            routes: vec![
                route("root", PathMatch::NginxPrefix("/".into())),
                route("exact", PathMatch::Exact("/a".into())),
                route("prefix", PathMatch::NginxPrefix("/a".into())),
                route("segment", PathMatch::IngressPrefix("/api".into())),
                route("unicode", PathMatch::NginxPrefix("/汉字".into())),
            ],
        }];
        let index = Index::build(&hosts);
        for (path, expected) in [
            ("/", "root"),
            ("/a", "exact"),
            ("/abc", "prefix"),
            ("/api/v1", "segment"),
            ("/apix", "prefix"),
            ("/汉字/文", "unicode"),
            ("/汉/", "root"),
        ] {
            assert_eq!(
                index.route(&hosts, listener, "test", path).unwrap().id,
                expected
            );
        }
        let long = format!("/api/{}", "文".repeat(1000));
        assert_eq!(
            index.route(&hosts, listener, "test", &long).unwrap().id,
            "segment"
        );
    }
}
