use crate::{model::Endpoint, traffic::Key};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

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
    pool: Mutex<Pool>,
}
#[derive(Debug)]
struct Pool {
    endpoints: Vec<(Endpoint, Arc<State>)>,
    next_dns: Instant,
    dns_success: Instant,
    next_health: Instant,
}
#[derive(Debug, Default)]
struct State {
    active: AtomicUsize,
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
                .map(|e| (e, Arc::new(State::default())))
                .collect(),
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
            pool: Mutex::new(pool),
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
    pub fn select(self: &Arc<Self>, key: &str) -> Option<Lease> {
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if self.options.max_inflight > 0
            && self.active.load(Ordering::Relaxed) >= self.options.max_inflight
        {
            return None;
        }
        let now = Instant::now();
        let eligible: Vec<_> = pool
            .endpoints
            .iter()
            .filter(|(_, state)| {
                let mut health = state.health.lock().unwrap_or_else(|e| e.into_inner());
                if health.blocked_until.is_some_and(|until| until <= now) {
                    health.blocked_until = None;
                    health.failures = 0;
                }
                health.blocked_until.is_none()
                    && (self.options.health.is_none() || health.active_ok == Some(true))
            })
            .collect();
        let total: u64 = eligible.iter().map(|(e, _)| u64::from(e.weight)).sum();
        if total == 0 {
            return None;
        }
        let chosen = match &self.options.balance {
            Balance::RoundRobin => {
                let mut n = self.cursor.fetch_add(1, Ordering::Relaxed) % total;
                eligible
                    .iter()
                    .find(|(e, _)| {
                        if n < u64::from(e.weight) {
                            true
                        } else {
                            n -= u64::from(e.weight);
                            false
                        }
                    })
                    .copied()
            }
            Balance::LeastConnections => {
                // Rotate ties to avoid concentrating fresh requests on one endpoint.
                let offset = self.cursor.fetch_add(1, Ordering::Relaxed) as usize % eligible.len();
                (0..eligible.len())
                    .map(|i| eligible[(i + offset) % eligible.len()])
                    .min_by(|(a, sa), (b, sb)| {
                        (sa.active.load(Ordering::Relaxed) as u64 * u64::from(b.weight))
                            .cmp(&(sb.active.load(Ordering::Relaxed) as u64 * u64::from(a.weight)))
                    })
            }
            Balance::Hash(_) | Balance::Sticky(_) => eligible
                .iter()
                .min_by(|(a, _), (b, _)| hash_score(key, a).total_cmp(&hash_score(key, b)))
                .copied(),
        }?;
        chosen.1.active.fetch_add(1, Ordering::Relaxed);
        self.active.fetch_add(1, Ordering::Relaxed);
        Some(Lease {
            address: chosen.0.address,
            state: chosen.1.clone(),
            backend: self.clone(),
        })
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
        let mut health = state.health.lock().unwrap_or_else(|e| e.into_inner());
        if !failed {
            health.failures = 0;
            health.blocked_until = None;
            return false;
        }
        if max_fails == 0 {
            return false;
        }
        health.failures = health.failures.saturating_add(1);
        if health.failures >= max_fails && health.blocked_until.is_none() {
            health.blocked_until = Some(Instant::now() + cooldown);
            return true;
        }
        false
    }
    pub fn diagnostic(&self) -> serde_json::Value {
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({"tls":self.tls,"hostname":self.hostname,"options":self.options,"origins":self.origins,"active":self.active.load(Ordering::Relaxed),
            "endpoints":pool.endpoints.iter().map(|(e,s)| { let h=s.health.lock().unwrap_or_else(|e|e.into_inner()); serde_json::json!({"address":e.address,"weight":e.weight,"active":s.active.load(Ordering::Relaxed),"failures":h.failures,"active_health":h.active_ok,"ejected":h.blocked_until.is_some_and(|t|t>Instant::now())}) }).collect::<Vec<_>>()})
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
                    .drain(..)
                    .map(|(e, s)| (e.address, s))
                    .collect();
                pool.endpoints = endpoints
                    .into_iter()
                    .map(|(address, weight)| {
                        (
                            Endpoint { address, weight },
                            previous.get(&address).cloned().unwrap_or_default(),
                        )
                    })
                    .collect();
                pool.next_dns = until.max(now + Duration::from_secs(1));
                pool.dns_success = now;
            } else if now.duration_since(pool.dns_success) > Duration::from_secs(60) {
                pool.endpoints.clear();
            }
        }
        if let Some(check) = health {
            let endpoints = self
                .pool
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .endpoints
                .clone();
            use futures::{StreamExt, stream};
            let mut checks = stream::iter(endpoints.into_iter().map(|(endpoint, state)| {
                let check = &check;
                async move {
                    let ok = self.probe(endpoint.address, check).await;
                    state
                        .health
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .active_ok = Some(ok);
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
        let url = format!(
            "{}://{host}:{}{}",
            if self.tls { "https" } else { "http" },
            address.port(),
            check.path
        );
        let Ok(client) = self.profile.client_builder() else {
            return false;
        };
        let client = client
            .timeout(check.timeout)
            .resolve(name, address)
            .pool_max_idle_per_host(0);
        let Ok(client) = client.build() else {
            return false;
        };
        client
            .get(url)
            .header("Host", &self.host_header)
            .send()
            .await
            .is_ok_and(|r| r.status().as_u16() == check.status)
    }
}

fn hash_score(key: &str, endpoint: &Endpoint) -> f64 {
    let bytes = Sha256::digest(format!("{key}\0{}", endpoint.address).as_bytes());
    let value = u64::from_be_bytes(bytes[..8].try_into().unwrap());
    let uniform = (value as f64 + 1.0) / (u64::MAX as f64 + 2.0);
    -uniform.ln() / f64::from(endpoint.weight)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn dns_ttl_refresh_replaces_endpoints_without_invalidating_active_leases() -> Result<()> {
        use hickory_resolver::{
            config::{LookupIpStrategy, NameServerConfigGroup, ResolverConfig},
            name_server::TokioConnectionProvider,
        };
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let address = socket.local_addr()?;
        let generation = Arc::new(AtomicUsize::new(1));
        let answer = generation.clone();
        let task = tokio::spawn(async move {
            let mut buffer = [0; 512];
            loop {
                let Ok((length, peer)) = socket.recv_from(&mut buffer).await else {
                    break;
                };
                let mut end = 12;
                while buffer[end] != 0 {
                    end += 1 + usize::from(buffer[end]);
                }
                end += 5;
                if end > length {
                    continue;
                }
                let mut response = buffer[..end].to_vec();
                response[2..12].copy_from_slice(&[0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]);
                response.extend_from_slice(&[
                    0xc0,
                    0x0c,
                    0,
                    1,
                    0,
                    1,
                    0,
                    0,
                    0,
                    1,
                    0,
                    4,
                    127,
                    0,
                    0,
                    answer.load(Ordering::Relaxed) as u8,
                ]);
                let _ = socket.send_to(&response, peer).await;
            }
        });
        let config = ResolverConfig::from_parts(
            None,
            vec![],
            NameServerConfigGroup::from_ips_clear(&[address.ip()], address.port(), true),
        );
        let mut builder = hickory_resolver::Resolver::builder_with_config(
            config,
            TokioConnectionProvider::default(),
        );
        builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4Only;
        let resolver = builder.build();
        let mut backend = Backend::new(vec![], false, "backend.test".into(), "backend.test".into());
        backend.origins = vec![Origin {
            host: "backend.test".into(),
            port: 8080,
            weight: 1,
        }];
        let backend = Arc::new(backend);
        backend.maintain(Some(&resolver)).await;
        let lease = backend.select("").expect("initial DNS answer");
        assert_eq!(lease.address, "127.0.0.1:8080".parse()?);
        generation.store(2, Ordering::Relaxed);
        backend.maintain(Some(&resolver)).await;
        assert_eq!(backend.select("").unwrap().address, lease.address);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        backend.maintain(Some(&resolver)).await;
        assert_eq!(
            backend.select("").unwrap().address,
            "127.0.0.2:8080".parse()?
        );
        assert_eq!(lease.address, "127.0.0.1:8080".parse()?);
        drop(lease);
        assert_eq!(backend.active.load(Ordering::Relaxed), 0);
        task.abort();
        Ok(())
    }
}
