use super::{Entry, ListenerRoutes, PathMatch, RequestData, Route};
use std::{
    borrow::Cow, cell::OnceCell, cmp::Ordering, collections::BTreeMap, net::SocketAddr, sync::Arc,
};

type Query<'a> = Vec<(Cow<'a, str>, Cow<'a, str>)>;

pub(super) struct Input<'a> {
    pub request: &'a RequestData,
    query: OnceCell<Query<'a>>,
}

impl<'a> Input<'a> {
    pub fn query(&self) -> &Query<'a> {
        self.query
            .get_or_init(|| url::form_urlencoded::parse(self.request.query.as_bytes()).collect())
    }
}

#[derive(Default)]
pub(super) struct Index {
    addresses: BTreeMap<SocketAddr, Hosts>,
    listeners: Vec<ListenerIndex>,
}

#[derive(Default)]
struct Hosts {
    exact: BTreeMap<String, usize>,
    wildcard: BTreeMap<String, usize>,
    fallback: Option<usize>,
}

impl Hosts {
    fn insert(&mut self, pattern: &str, id: usize) {
        if pattern.is_empty() {
            self.fallback = Some(id);
        } else {
            self.exact.insert(pattern.into(), id);
            if let Some(suffix) = pattern.strip_prefix("*.") {
                self.wildcard.insert(suffix.into(), id);
            }
        }
    }

    fn matching<'a>(&'a self, host: &'a str) -> impl Iterator<Item = usize> + 'a {
        self.exact
            .get(host)
            .copied()
            .into_iter()
            .chain(host.match_indices('.').filter_map(|(dot, _)| {
                (dot > 0)
                    .then(|| self.wildcard.get(&host[dot + 1..]).copied())
                    .flatten()
            }))
            .chain(self.fallback)
    }
}

#[derive(Default)]
struct Paths {
    exact: BTreeMap<String, Vec<usize>>,
    prefix: BTreeMap<String, Vec<usize>>,
    other: Vec<usize>,
}

struct Predicate {
    priority: (bool, usize, usize, usize, usize),
    headers: Vec<String>,
}

struct ListenerIndex {
    hosts: Hosts,
    paths: Vec<Paths>,
    predicates: Vec<Predicate>,
}

impl Index {
    pub fn build(listeners: &[ListenerRoutes]) -> Self {
        let mut index = Self::default();
        for (id, listener) in listeners.iter().enumerate() {
            // Equal listener ranks historically select the last listener,
            // even when it has no matching route. Do not fall back from it.
            index
                .addresses
                .entry(listener.address)
                .or_default()
                .insert(&listener.hostname, id);
            index
                .listeners
                .push(ListenerIndex::build(&listener.entries));
        }
        index
    }

    pub fn route(
        &self,
        listeners: &[ListenerRoutes],
        address: SocketAddr,
        request: &RequestData,
    ) -> Option<Arc<Route>> {
        let host = if request.host.bytes().any(|c| c.is_ascii_uppercase()) {
            Cow::Owned(request.host.to_ascii_lowercase())
        } else {
            Cow::Borrowed(request.host.as_str())
        };
        let id = self.addresses.get(&address)?.matching(&host).next()?;
        let listener = &self.listeners[id];
        let entries = &listeners[id].entries;
        let input = Input {
            request,
            query: OnceCell::new(),
        };
        if let [entry] = entries.as_slice() {
            return (super::host_rank(&entry.hostname, &host).is_some()
                && entry
                    .matcher
                    .matches(entry, &input, &listener.predicates[0].headers))
            .then(|| entry.route.clone());
        }
        for host_id in listener.hosts.matching(&host) {
            let paths = &listener.paths[host_id];
            let mut best = None;
            if let Some(ids) = paths.exact.get(&request.path) {
                listener.select(ids, entries, &input, &mut best);
            }
            for prefix in std::iter::once(request.path.as_str()).chain(
                request
                    .path
                    .match_indices('/')
                    .rev()
                    .filter(|(slash, _)| *slash > 0)
                    .map(|(slash, _)| &request.path[..slash]),
            ) {
                if let Some(ids) = paths.prefix.get(prefix) {
                    listener.select(ids, entries, &input, &mut best);
                }
            }
            listener.select(&paths.other, entries, &input, &mut best);
            if let Some(id) = best {
                return Some(entries[id].route.clone());
            }
        }
        None
    }
}

impl ListenerIndex {
    fn build(entries: &[Entry]) -> Self {
        let mut groups: BTreeMap<&str, Paths> = BTreeMap::new();
        let predicates = entries
            .iter()
            .enumerate()
            .map(|(id, entry)| {
                let paths = groups.entry(&entry.hostname).or_default();
                match &entry.route.matcher {
                    PathMatch::Exact(path) => paths.exact.entry(path.clone()).or_default(),
                    PathMatch::IngressPrefix(path) if !path.trim_end_matches('/').is_empty() => {
                        paths
                            .prefix
                            .entry(path.trim_end_matches('/').into())
                            .or_default()
                    }
                    _ => &mut paths.other,
                }
                .push(id);
                Predicate {
                    priority: entry.priority(),
                    headers: entry
                        .matcher
                        .headers
                        .iter()
                        .map(|h| h.name.to_ascii_lowercase())
                        .collect(),
                }
            })
            .collect();
        let mut index = Self {
            hosts: Hosts::default(),
            paths: Vec::with_capacity(groups.len()),
            predicates,
        };
        for (host, mut paths) in groups {
            for ids in paths
                .exact
                .values_mut()
                .chain(paths.prefix.values_mut())
                .chain(std::iter::once(&mut paths.other))
            {
                ids.sort_unstable_by(|a, b| index.compare(*b, *a, entries));
            }
            index.hosts.insert(host, index.paths.len());
            index.paths.push(paths);
        }
        index
    }

    fn compare(&self, a: usize, b: usize, entries: &[Entry]) -> Ordering {
        self.predicates[a]
            .priority
            .cmp(&self.predicates[b].priority)
            .then_with(|| entries[b].order.cmp(&entries[a].order))
            .then_with(|| a.cmp(&b))
    }

    fn select(
        &self,
        ids: &[usize],
        entries: &[Entry],
        input: &Input<'_>,
        best: &mut Option<usize>,
    ) {
        for &id in ids {
            if best.is_some_and(|previous| !self.compare(id, previous, entries).is_gt()) {
                break;
            }
            let entry = &entries[id];
            if entry
                .matcher
                .matches(entry, input, &self.predicates[id].headers)
            {
                *best = Some(id);
                break;
            }
        }
    }
}
