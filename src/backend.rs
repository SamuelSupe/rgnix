use crate::{model::Endpoint, traffic::Key};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

mod selection;
use selection::Selection;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub enum Balance {
    #[default]
    RoundRobin,
    LeastConnections,
    Hash(Key),
    Sticky(String),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HealthCheck {
    pub path: String,
    pub interval: Duration,
    pub timeout: Duration,
    pub status: u16,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Options {
    pub balance: Balance,
    pub max_inflight: usize,
    pub health: Option<HealthCheck>,
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_inflight <= 1_000_000,
            "backend concurrency must be 0..1000000"
        );
        if let Some(h) = &self.health {
            ensure!(
                h.path.starts_with('/')
                    && h.path.len() <= 1024
                    && !h.path.starts_with("//")
                    && !h.path.contains(['\r', '\n', '#']),
                "invalid health check path"
            );
            ensure!(
                (1..=3600).contains(&h.interval.as_secs())
                    && !h.timeout.is_zero()
                    && h.timeout <= Duration::from_secs(30),
                "health interval must be 1s..1h and timeout 1ms..30s"
            );
            ensure!(
                (200..=399).contains(&h.status),
                "health check expected status must be 200..399"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    pub host: String,
    pub port: u16,
    pub weight: u32,
}

#[derive(Debug)]
pub struct Backend {
    pub endpoints: Vec<Endpoint>,
    pub tls: bool,
    pub hostname: String,
    pub host_header: String,
    pub options: Options,
    pub origins: Vec<Origin>,
    pub ca_pem: Option<String>,
    pub profile: crate::upstream::Transport,
    cursor: AtomicU64,
    active: AtomicUsize,
    health_revision: AtomicU64,
    pool: Mutex<Pool>,
    http_probe_client: OnceLock<reqwest::Client>,
}
#[derive(Debug)]
struct Pool {
    endpoints: Arc<[(Endpoint, Arc<State>)]>,
    selection: Option<Arc<Selection>>,
    next_dns: Instant,
    dns_success: Instant,
    next_health: Instant,
}
#[derive(Debug)]
struct State {
    hash_address: String,
    active: AtomicUsize,
    // False implies no passive failures or cooldown; transitions hold the health lock.
    has_failures: AtomicBool,
    health: Mutex<Health>,
}
#[derive(Debug, Default)]
struct Health {
    failures: usize,
    blocked_until: Option<Instant>,
    active_ok: Option<bool>,
}
pub struct Lease {
    pub address: SocketAddr,
    state: Arc<State>,
    backend: Arc<Backend>,
}
impl Lease {
    pub fn record_result(&self, failed: bool, max_fails: usize, cooldown: Duration) -> bool {
        self.state
            .record_result(failed, max_fails, cooldown, &self.backend.health_revision)
    }
}

impl State {
    fn new(address: SocketAddr) -> Self {
        Self {
            hash_address: address.to_string(),
            active: AtomicUsize::new(0),
            has_failures: AtomicBool::new(false),
            health: Mutex::new(Health::default()),
        }
    }

    fn record_result(
        &self,
        failed: bool,
        max_fails: usize,
        cooldown: Duration,
        revision: &AtomicU64,
    ) -> bool {
        if (!failed && !self.has_failures.load(Ordering::Acquire)) || (failed && max_fails == 0) {
            return false;
        }
        let mut health = self.health.lock().unwrap_or_else(|e| e.into_inner());
        if !failed {
            health.failures = 0;
            if health.blocked_until.take().is_some() {
                revision.fetch_add(1, Ordering::Release);
            }
            self.has_failures.store(false, Ordering::Release);
            return false;
        }
        health.failures = health.failures.saturating_add(1);
        self.has_failures.store(true, Ordering::Release);
        if health.failures >= max_fails && health.blocked_until.is_none() {
            health.blocked_until = Some(Instant::now() + cooldown);
            revision.fetch_add(1, Ordering::Release);
            return true;
        }
        false
    }

    fn set_active_health(&self, ok: bool, revision: &AtomicU64) {
        let mut health = self.health.lock().unwrap_or_else(|e| e.into_inner());
        if health.active_ok != Some(ok) {
            health.active_ok = Some(ok);
            revision.fetch_add(1, Ordering::Release);
        }
    }
}
pub(crate) struct Observation {
    pub endpoints: usize,
    pub eligible: usize,
    pub ejected: usize,
    pub unready: usize,
    pub active: usize,
    pub dns_age: Option<f64>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::Relaxed);
        self.backend.active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Backend {
    pub fn new(endpoints: Vec<Endpoint>, tls: bool, hostname: String, host_header: String) -> Self {
        let now = Instant::now();
        let pool = Pool {
            endpoints: endpoints
                .iter()
                .cloned()
                .map(|e| {
                    let state = Arc::new(State::new(e.address));
                    (e, state)
                })
                .collect(),
            selection: None,
            next_dns: now,
            dns_success: now,
            next_health: now,
        };
        Self {
            endpoints,
            tls,
            hostname,
            host_header,
            options: Options::default(),
            origins: vec![],
            ca_pem: None,
            profile: Default::default(),
            cursor: AtomicU64::new(0),
            active: AtomicUsize::new(0),
            health_revision: AtomicU64::new(0),
            pool: Mutex::new(pool),
            http_probe_client: OnceLock::new(),
        }
    }
    pub fn transport(&self, tls: bool, hostname: String, host_header: String) -> Self {
        let mut result = Self::new(self.endpoints.clone(), tls, hostname, host_header);
        result.options = self.options.clone();
        result.origins = self.origins.clone();
        result.ca_pem = self.ca_pem.clone();
        result.profile = self.profile.clone();
        result
    }
    pub fn same_config(&self, previous: &Self) -> bool {
        self.endpoints == previous.endpoints
            && self.tls == previous.tls
            && self.hostname == previous.hostname
            && self.host_header == previous.host_header
            && self.options == previous.options
            && self.origins == previous.origins
            && self.ca_pem == previous.ca_pem
            && self.profile.pool_key() == previous.profile.pool_key()
    }
    pub fn record_result(
        &self,
        address: SocketAddr,
        failed: bool,
        max_fails: usize,
        cooldown: Duration,
    ) -> bool {
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, state)) = pool.endpoints.iter().find(|(e, _)| e.address == address) else {
            return false;
        };
        state.record_result(failed, max_fails, cooldown, &self.health_revision)
    }
    pub fn diagnostic(&self) -> serde_json::Value {
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({"tls":self.tls,"hostname":self.hostname,"options":self.options,"origins":self.origins,"active":self.active.load(Ordering::Relaxed),
            "endpoints":pool.endpoints.iter().map(|(e,s)| { let h=s.health.lock().unwrap_or_else(|e|e.into_inner()); serde_json::json!({"address":e.address,"weight":e.weight,"active":s.active.load(Ordering::Relaxed),"failures":h.failures,"active_health":h.active_ok,"ejected":h.blocked_until.is_some_and(|t|t>Instant::now())}) }).collect::<Vec<_>>()})
    }
    pub(crate) fn observation(&self) -> Observation {
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let mut result = Observation {
            endpoints: pool.endpoints.len(),
            eligible: 0,
            ejected: 0,
            unready: 0,
            active: self.active.load(Ordering::Relaxed),
            dns_age: self
                .origins
                .iter()
                .any(|o| o.host.parse::<std::net::IpAddr>().is_err())
                .then(|| pool.dns_success.elapsed().as_secs_f64()),
        };
        for (endpoint, state) in pool.endpoints.iter() {
            let health = state.health.lock().unwrap_or_else(|e| e.into_inner());
            let ejected = health.blocked_until.is_some_and(|until| until > now);
            let unready = self.options.health.is_some() && health.active_ok != Some(true);
            result.ejected += usize::from(ejected);
            result.unready += usize::from(unready);
            result.eligible += usize::from(!ejected && !unready && endpoint.weight > 0);
        }
        result
    }
    pub async fn maintain(&self, resolver: Option<&hickory_resolver::TokioResolver>) {
        let now = Instant::now();
        let (dns, health) = {
            let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            let dns = self
                .origins
                .iter()
                .any(|origin| origin.host.parse::<std::net::IpAddr>().is_err())
                && now >= pool.next_dns;
            let health = self
                .options
                .health
                .as_ref()
                .filter(|_| now >= pool.next_health)
                .cloned();
            if dns {
                pool.next_dns = now + Duration::from_secs(5);
            }
            if let Some(h) = &health {
                pool.next_health = now + h.interval;
            }
            (dns, health)
        };
        if dns && let Some(resolver) = resolver {
            let mut endpoints = BTreeMap::new();
            let mut until = now + Duration::from_secs(3600);
            let mut failed = false;
            for origin in &self.origins {
                if let Ok(ip) = origin.host.parse() {
                    endpoints.insert(SocketAddr::new(ip, origin.port), origin.weight);
                    continue;
                }
                match tokio::time::timeout(Duration::from_secs(2), resolver.lookup_ip(&origin.host))
                    .await
                {
                    Ok(Ok(lookup)) => {
                        until = until.min(lookup.valid_until());
                        for ip in lookup.iter() {
                            endpoints.insert(SocketAddr::new(ip, origin.port), origin.weight);
                        }
                    }
                    _ => {
                        failed = true;
                        break;
                    }
                }
            }
            let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            if !failed {
                let previous: BTreeMap<_, _> = pool
                    .endpoints
                    .iter()
                    .map(|(e, s)| (e.address, s.clone()))
                    .collect();
                pool.endpoints = endpoints
                    .into_iter()
                    .map(|(address, weight)| {
                        (
                            Endpoint { address, weight },
                            previous
                                .get(&address)
                                .cloned()
                                .unwrap_or_else(|| Arc::new(State::new(address))),
                        )
                    })
                    .collect();
                pool.selection = None;
                pool.next_dns = until.max(now + Duration::from_secs(1));
                pool.dns_success = now;
            } else if now.duration_since(pool.dns_success) > Duration::from_secs(60) {
                pool.endpoints = Arc::from([]);
                pool.selection = None;
            }
        }
        if let Some(check) = health {
            let endpoints = self
                .pool
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .endpoints
                .to_vec();
            use futures::{StreamExt, stream};
            let mut checks = stream::iter(endpoints.into_iter().map(|(endpoint, state)| {
                let check = &check;
                async move {
                    let ok = self.probe(endpoint.address, check).await;
                    state.set_active_health(ok, &self.health_revision);
                }
            }))
            .buffer_unordered(8);
            while checks.next().await.is_some() {}
        }
    }
    async fn probe(&self, address: SocketAddr, check: &HealthCheck) -> bool {
        let name = self
            .profile
            .server_name
            .as_deref()
            .unwrap_or(&self.hostname);
        let host = if name.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{name}]")
        } else {
            name.into()
        };
        let url = if self.tls {
            format!("https://{host}:{}{}", address.port(), check.path)
        } else {
            format!("http://{address}{}", check.path)
        };
        let Some(client) = self.probe_client(name, address) else {
            return false;
        };
        client
            .get(url)
            .header("Host", &self.host_header)
            .timeout(check.timeout)
            .send()
            .await
            .is_ok_and(|r| r.status().as_u16() == check.status)
    }

    fn probe_client(&self, name: &str, address: SocketAddr) -> Option<reqwest::Client> {
        if !self.tls
            && let Some(client) = self.http_probe_client.get()
        {
            return Some(client.clone());
        }
        let Ok(client) = self.profile.client_builder() else {
            return None;
        };
        let mut client = client.pool_max_idle_per_host(0);
        if self.tls {
            // HTTPS needs the configured name for SNI and verification, while
            // the resolver override keeps this probe on the selected endpoint.
            client = client.resolve(name, address);
        }
        let client = client.build().ok()?;
        if !self.tls {
            // HTTP URLs carry the target IP, so one client can probe every
            // endpoint without pinning future DNS generations to an old address.
            let _ = self.http_probe_client.set(client.clone());
        };
        Some(client)
    }
}

#[cfg(test)]
mod tests;
