pub(crate) mod global;
use crate::script::RequestData;
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    pub rate: Option<Rate>,
    pub concurrency: Option<Concurrency>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Rate {
    pub per_second: u32,
    pub burst: u32,
    pub key: Key,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Concurrency {
    pub limit: u32,
    pub key: Key,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Key {
    Route,
    Ip,
    Header(String),
    Cookie(String),
    Claim(String),
}

impl Key {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "route" => Ok(Self::Route),
            "ip" => Ok(Self::Ip),
            _ => {
                let (kind, value) = value.split_once(':').ok_or_else(|| {
                    anyhow::anyhow!("key expects route, ip, header:NAME, cookie:NAME or jwt:CLAIM")
                })?;
                ensure!(!value.is_empty() && value.len() <= 128, "invalid key name");
                Ok(match kind {
                    "header" => {
                        http::header::HeaderName::from_bytes(value.as_bytes())?;
                        Self::Header(value.to_ascii_lowercase())
                    }
                    "cookie" => Self::Cookie(value.into()),
                    "jwt" => Self::Claim(value.into()),
                    _ => bail!("unsupported key kind {kind}"),
                })
            }
        }
    }
    pub fn value(
        &self,
        request: &RequestData,
        claims: &std::collections::BTreeMap<String, String>,
    ) -> String {
        match self {
            Self::Route => String::new(),
            Self::Ip => request.remote_addr.clone(),
            Self::Header(name) => request.headers.get(name).cloned().unwrap_or_default(),
            Self::Claim(name) => claims.get(name).cloned().unwrap_or_default(),
            Self::Cookie(name) => request
                .headers
                .get("cookie")
                .and_then(|v| {
                    v.split(';')
                        .filter_map(|p| p.trim().split_once('='))
                        .find(|(k, _)| k == name)
                        .map(|(_, v)| v.to_owned())
                })
                .unwrap_or_default(),
        }
    }
}
impl Policy {
    pub fn phase(&self, before_auth: bool) -> Self {
        let before = |key: &Key| matches!(key, Key::Ip | Key::Route);
        Self {
            rate: self.rate.clone().filter(|r| before(&r.key) == before_auth),
            concurrency: self
                .concurrency
                .clone()
                .filter(|c| before(&c.key) == before_auth),
        }
    }
    pub fn validate(&self) -> Result<()> {
        if let Some(r) = &self.rate {
            ensure!(
                (1..=1_000_000).contains(&r.per_second) && (1..=1_000_000).contains(&r.burst),
                "rate and burst must be 1..1000000"
            );
        }
        if let Some(c) = &self.concurrency {
            ensure!(
                (1..=1_000_000).contains(&c.limit),
                "concurrency must be 1..1000000"
            );
        }
        Ok(())
    }
}

pub struct Limiter {
    capacity: std::sync::atomic::AtomicUsize,
    buckets: Mutex<HashMap<[u8; 32], Bucket>>,
}
struct Bucket {
    tokens: f64,
    active: u32,
    touched: Instant,
}
pub struct Permit {
    limiter: Arc<Limiter>,
    key: [u8; 32],
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(bucket) = self
            .limiter
            .buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.key)
        {
            bucket.active = bucket.active.saturating_sub(1);
            bucket.touched = Instant::now();
        }
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self {
            capacity: std::sync::atomic::AtomicUsize::new(16384),
            buckets: Default::default(),
        }
    }
}
impl Limiter {
    pub(crate) fn observation(&self) -> (usize, usize) {
        (
            self.buckets.lock().unwrap_or_else(|e| e.into_inner()).len(),
            self.capacity.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
    pub fn set_capacity(&self, value: usize) {
        self.capacity
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn acquire(
        self: &Arc<Self>,
        route: &str,
        policy: &Policy,
        request: &RequestData,
        claims: &std::collections::BTreeMap<String, String>,
    ) -> std::result::Result<Option<Permit>, u16> {
        let capacity = self.capacity.load(std::sync::atomic::Ordering::Relaxed);
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() >= capacity {
            buckets.retain(|_, b| {
                b.active > 0 || now.duration_since(b.touched) < Duration::from_secs(60)
            });
        }
        let key = |kind: &str, selector: &Key| -> [u8; 32] {
            let policy = serde_json::to_string(policy).unwrap();
            Sha256::digest(
                format!(
                    "{route}\0{kind}\0{policy}\0{}",
                    selector.value(request, claims)
                )
                .as_bytes(),
            )
            .into()
        };
        let mut permit_key = None;
        if let Some(c) = &policy.concurrency {
            let id = key("concurrency", &c.key);
            if !buckets.contains_key(&id) && buckets.len() >= capacity {
                return Err(503);
            }
            let bucket = buckets.entry(id).or_insert(Bucket {
                tokens: 0.0,
                active: 0,
                touched: now,
            });
            if bucket.active >= c.limit {
                return Err(503);
            }
            permit_key = Some(id);
        }
        if let Some(r) = &policy.rate {
            let id = key("rate", &r.key);
            if !buckets.contains_key(&id) && buckets.len() >= capacity {
                return Err(503);
            }
            let bucket = buckets.entry(id).or_insert(Bucket {
                tokens: r.burst as f64,
                active: 0,
                touched: now,
            });
            bucket.tokens = (bucket.tokens
                + now.duration_since(bucket.touched).as_secs_f64() * r.per_second as f64)
                .min(r.burst as f64);
            bucket.touched = now;
            if bucket.tokens < 1.0 {
                return Err(429);
            }
            bucket.tokens -= 1.0;
        }
        Ok(permit_key.map(|key| {
            let bucket = buckets.get_mut(&key).unwrap();
            bucket.active += 1;
            bucket.touched = now;
            Permit {
                limiter: self.clone(),
                key,
            }
        }))
    }
}
