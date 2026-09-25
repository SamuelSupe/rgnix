pub(crate) mod domains;
use anyhow::{Result, ensure};
use arc_swap::ArcSwap;
pub use domains::{Domains, namespace_name};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Quota {
    pub max_ingresses: usize,
    pub max_routes: usize,
    pub max_backends: usize,
    pub max_body_bytes: u64,
    pub max_timeout_seconds: u64,
    pub max_script_bytes: usize,
    pub max_inflight: usize,
    pub max_plugins: usize,
    pub max_auth: usize,
    pub max_mirrors: usize,
    pub max_limiter_keys: usize,
    pub requests_per_second: u32,
    pub burst: u32,
    pub allow_scripts: bool,
    pub allow_mirroring: bool,
}
impl Default for Quota {
    fn default() -> Self {
        Self {
            max_ingresses: 32,
            max_routes: 128,
            max_backends: 64,
            max_body_bytes: 16 * 1024 * 1024,
            max_timeout_seconds: 60,
            max_script_bytes: 256 * 1024,
            max_inflight: 64,
            max_plugins: 4,
            max_auth: 16,
            max_mirrors: 4,
            max_limiter_keys: 2048,
            requests_per_second: 1000,
            burst: 1000,
            allow_scripts: true,
            allow_mirroring: false,
        }
    }
}
impl Quota {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=10000).contains(&self.max_ingresses)
                && (1..=100000).contains(&self.max_routes)
                && (1..=10000).contains(&self.max_backends),
            "invalid namespace configuration quota"
        );
        ensure!(
            self.max_body_bytes > 0
                && self.max_body_bytes <= 1024 * 1024 * 1024
                && (1..=3600).contains(&self.max_timeout_seconds)
                && self.max_script_bytes <= 1024 * 1024,
            "invalid namespace body/timeout/script ceiling"
        );
        ensure!(
            self.max_inflight > 0
                && self.max_inflight <= 100000
                && self.max_plugins <= self.max_inflight
                && self.max_auth <= self.max_inflight
                && self.max_mirrors <= self.max_inflight
                && (16..=16384).contains(&self.max_limiter_keys)
                && (1..=1_000_000).contains(&self.requests_per_second)
                && (1..=1_000_000).contains(&self.burst),
            "invalid namespace runtime quota"
        );
        Ok(())
    }
    pub fn constrain(&self, settings: &mut crate::model::Settings) {
        if settings.max_body == 0 || settings.max_body > self.max_body_bytes {
            settings.max_body = self.max_body_bytes;
        }
        let timeout = std::time::Duration::from_secs(self.max_timeout_seconds);
        settings.connect_timeout = settings.connect_timeout.min(timeout);
        settings.read_timeout = settings.read_timeout.min(timeout);
        settings.write_timeout = settings.write_timeout.min(timeout);
        settings.keepalive = settings.keepalive.min(timeout);
        settings.body_policy.timeout = settings.body_policy.timeout.min(timeout);
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub default: Option<Quota>,
    pub namespaces: BTreeMap<String, Quota>,
    pub domains: Option<Domains>,
}
impl Policy {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = crate::controls::read_bounded(path, 1024 * 1024)?;
        let policy: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            policy.namespaces.len() <= 1024,
            "at most 1024 namespace policies"
        );
        for (name, quota) in &policy.namespaces {
            ensure!(
                !name.is_empty()
                    && name.len() <= 63
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "invalid namespace name"
            );
            quota.validate()?;
        }
        if let Some(quota) = &policy.default {
            quota.validate()?;
        }
        if let Some(domains) = &policy.domains {
            domains.validate()?;
        }
        Ok(policy)
    }
    pub fn quota(&self, namespace: &str) -> Option<&Quota> {
        self.namespaces.get(namespace).or(self.default.as_ref())
    }
}
#[derive(Default)]
pub struct Tenants {
    states: Mutex<BTreeMap<String, Arc<Tenant>>>,
}
pub struct Tenant {
    pub name: String,
    pub quota: ArcSwap<Quota>,
    pub traffic: Arc<crate::traffic::Limiter>,
    active: Mutex<[usize; 4]>,
}
#[derive(Clone, Copy)]
pub enum Resource {
    Request,
    Plugin,
    Auth,
    Mirror,
}
pub struct Permit {
    tenant: Arc<Tenant>,
    resource: Resource,
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.tenant.active.lock().unwrap_or_else(|e| e.into_inner())[self.resource as usize] -= 1;
    }
}
impl Tenant {
    pub fn acquire(self: &Arc<Self>, resource: Resource) -> std::result::Result<Permit, u16> {
        let quota = self.quota.load();
        let limit = match resource {
            Resource::Request => quota.max_inflight,
            Resource::Plugin => quota.max_plugins,
            Resource::Auth => quota.max_auth,
            Resource::Mirror => quota.max_mirrors,
        };
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if active[resource as usize] >= limit {
            return Err(503);
        }
        active[resource as usize] += 1;
        Ok(Permit {
            tenant: self.clone(),
            resource,
        })
    }
    pub fn rate(
        self: &Arc<Self>,
        request: &crate::script::RequestData,
    ) -> std::result::Result<(), u16> {
        let quota = self.quota.load();
        self.traffic.acquire(
            "_namespace",
            &crate::traffic::Policy {
                rate: Some(crate::traffic::Rate {
                    per_second: quota.requests_per_second,
                    burst: quota.burst,
                    key: crate::traffic::Key::Route,
                }),
                concurrency: None,
            },
            request,
            &request.claims,
        )?;
        Ok(())
    }
    pub fn diagnostic(&self) -> serde_json::Value {
        serde_json::json!({"namespace":self.name,"quota":**self.quota.load(),"active":*self.active.lock().unwrap_or_else(|e|e.into_inner())})
    }
}
impl Tenants {
    pub fn tenant(&self, namespace: &str, quota: &Quota) -> Arc<Tenant> {
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let tenant = states
            .entry(namespace.into())
            .or_insert_with(|| {
                Arc::new(Tenant {
                    name: namespace.into(),
                    quota: ArcSwap::from_pointee(quota.clone()),
                    traffic: Arc::new(crate::traffic::Limiter::default()),
                    active: Mutex::new([0; 4]),
                })
            })
            .clone();
        tenant.quota.store(Arc::new(quota.clone()));
        tenant.traffic.set_capacity(quota.max_limiter_keys);
        tenant
    }
    pub fn retain(&self, names: &std::collections::BTreeSet<String>) {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|name, tenant| names.contains(name) || Arc::strong_count(tenant) > 1);
    }
}
