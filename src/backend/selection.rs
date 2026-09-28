use super::*;
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub(super) struct Selection {
    endpoints: Vec<(Endpoint, Arc<State>)>,
    cumulative_weights: Vec<u64>,
    revision: u64,
    expires: Option<Instant>,
}

impl Selection {
    fn new(backend: &Backend, endpoints: &[(Endpoint, Arc<State>)], revision: u64) -> Self {
        let mut result = Self {
            endpoints: Vec::with_capacity(endpoints.len()),
            cumulative_weights: Vec::with_capacity(endpoints.len()),
            revision,
            expires: None,
        };
        let mut total = 0;
        for entry in endpoints {
            if backend.ready(entry, &mut result.expires) {
                total += u64::from(entry.0.weight);
                result.cumulative_weights.push(total);
                result.endpoints.push(entry.clone());
            }
        }
        result
    }
}

impl Backend {
    pub fn select(self: &Arc<Self>, key: &str) -> Option<Lease> {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if self.options.max_inflight > 0
            && self.active.load(Ordering::Relaxed) >= self.options.max_inflight
        {
            return None;
        }
        if let [entry] = pool.endpoints.as_ref() {
            if !self.ready(entry, &mut None) {
                return None;
            }
            return self.lease(entry);
        }
        let revision = self.health_revision.load(Ordering::Acquire);
        if pool.selection.as_ref().is_none_or(|selection| {
            selection.revision != revision
                || selection
                    .expires
                    .is_some_and(|until| until <= Instant::now())
        }) {
            // Read the revision before scanning. A concurrent health change must
            // invalidate this snapshot, even if the scan has already visited its endpoint.
            pool.selection = Some(Arc::new(Selection::new(self, &pool.endpoints, revision)));
        }
        let selection = pool.selection.as_ref().unwrap();
        if matches!(self.options.balance, Balance::Hash(_) | Balance::Sticky(_)) {
            let selection = selection.clone();
            drop(pool);
            self.select_from(&selection, key)
        } else {
            // Least-connections selection and reservation stay under the same lock.
            self.select_from(selection, key)
        }
    }

    pub(super) fn select_from(self: &Arc<Self>, selection: &Selection, key: &str) -> Option<Lease> {
        let total = selection.cumulative_weights.last()?;
        let eligible = &selection.endpoints;
        let chosen = match &self.options.balance {
            Balance::RoundRobin => {
                let n = self.cursor.fetch_add(1, Ordering::Relaxed) % total;
                let index = selection
                    .cumulative_weights
                    .partition_point(|end| *end <= n);
                &eligible[index]
            }
            Balance::LeastConnections => {
                // Rotate ties to avoid concentrating fresh requests on one endpoint.
                let offset = self.cursor.fetch_add(1, Ordering::Relaxed) as usize % eligible.len();
                (0..eligible.len())
                    .map(|i| &eligible[(i + offset) % eligible.len()])
                    .min_by(|(a, sa), (b, sb)| {
                        (sa.active.load(Ordering::Relaxed) as u64 * u64::from(b.weight))
                            .cmp(&(sb.active.load(Ordering::Relaxed) as u64 * u64::from(a.weight)))
                    })?
            }
            Balance::Hash(_) | Balance::Sticky(_) => {
                let mut prefix = Sha256::new();
                prefix.update(key.as_bytes());
                prefix.update([0]);
                eligible
                    .iter()
                    .map(|entry| (entry, hash_score(&prefix, entry)))
                    .min_by(|(_, a), (_, b)| a.total_cmp(b))?
                    .0
            }
        };
        self.lease(chosen)
    }

    fn ready(
        &self,
        (endpoint, state): &(Endpoint, Arc<State>),
        expires: &mut Option<Instant>,
    ) -> bool {
        if endpoint.weight == 0 {
            return false;
        }
        if self.options.health.is_none() && !state.has_failures.load(Ordering::Acquire) {
            return true;
        }
        let mut health = state.health.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(until) = health.blocked_until {
            if until <= Instant::now() {
                health.blocked_until = None;
                health.failures = 0;
                state.has_failures.store(false, Ordering::Release);
            } else {
                *expires = Some(expires.map_or(until, |current| current.min(until)));
            }
        }
        health.blocked_until.is_none()
            && (self.options.health.is_none() || health.active_ok == Some(true))
    }

    fn lease(self: &Arc<Self>, chosen: &(Endpoint, Arc<State>)) -> Option<Lease> {
        if self.options.max_inflight == 0 {
            self.active.fetch_add(1, Ordering::Relaxed);
        } else {
            self.active
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                    (active < self.options.max_inflight).then_some(active + 1)
                })
                .ok()?;
        }
        chosen.1.active.fetch_add(1, Ordering::Relaxed);
        Some(Lease {
            address: chosen.0.address,
            state: chosen.1.clone(),
            backend: self.clone(),
        })
    }
}

fn hash_score(prefix: &Sha256, (endpoint, state): &(Endpoint, Arc<State>)) -> f64 {
    // Preserve the key\0textual-address digest so existing sticky sessions keep their backend.
    let mut hash = prefix.clone();
    hash.update(state.hash_address.as_bytes());
    let bytes = hash.finalize();
    let value = u64::from_be_bytes(bytes[..8].try_into().unwrap());
    let uniform = (value as f64 + 1.0) / (u64::MAX as f64 + 2.0);
    -uniform.ln() / f64::from(endpoint.weight)
}
