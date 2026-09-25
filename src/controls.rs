use anyhow::{Result, ensure};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, sync::Arc, time::Duration};

pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "control file exceeds size limit");
    Ok(bytes)
}
pub struct State {
    pub credentials: crate::diagnostics::auth::Credentials,
    pub tenant_policy: Option<crate::tenancy::Policy>,
    pub digest: String,
    pub metrics: crate::rollout::metrics::Metrics,
    pub global_rate: Option<Arc<crate::traffic::global::Global>>,
}
pub struct Controls {
    pub active: ArcSwap<State>,
    options: crate::diagnostics::Options,
    limits: crate::runtime::Limits,
}
impl Controls {
    pub fn new(
        options: crate::diagnostics::Options,
        limits: crate::runtime::Limits,
    ) -> Result<Self> {
        let state = Self::load(&options, &limits)?;
        Ok(Self {
            active: ArcSwap::from_pointee(state),
            options,
            limits,
        })
    }
    fn digest(
        options: &crate::diagnostics::Options,
        limits: &crate::runtime::Limits,
    ) -> Result<String> {
        let mut digest = Sha256::new();
        for path in [
            &options.admin_token_file,
            &options.admin_read_token_file,
            &options.admin_users_file,
            &limits.tenant_policy_file,
            &limits.rollout_metrics_file,
            &limits.global_rate_limit_file,
        ]
        .into_iter()
        .flatten()
        {
            digest.update(path.as_os_str().as_encoded_bytes());
            digest.update([0]);
            digest.update(read_bounded(path, 1024 * 1024)?);
        }
        Ok(format!("{:x}", digest.finalize()))
    }
    fn load(
        options: &crate::diagnostics::Options,
        limits: &crate::runtime::Limits,
    ) -> Result<State> {
        let digest = Self::digest(options, limits)?;
        let credentials = crate::diagnostics::auth::Credentials::load(options)?;
        let tenant_policy = limits
            .tenant_policy_file
            .as_ref()
            .map(|p| crate::tenancy::Policy::load(p))
            .transpose()?;
        if let Some(policy) = &tenant_policy {
            for quota in policy.default.iter().chain(policy.namespaces.values()) {
                ensure!(
                    quota.max_inflight < limits.max_inflight
                        && quota.max_plugins < limits.max_plugin_instances,
                    "namespace request/plugin quotas must be smaller than global budgets"
                );
            }
        }
        let metrics =
            crate::rollout::metrics::Metrics::load(limits.rollout_metrics_file.as_deref())?;
        let global_rate = limits
            .global_rate_limit_file
            .as_deref()
            .map(crate::traffic::global::Global::load)
            .transpose()?;
        ensure!(
            digest == Self::digest(options, limits)?,
            "control files changed during validation; retrying"
        );
        Ok(State {
            metrics,
            global_rate,
            credentials,
            tenant_policy,
            digest,
        })
    }
    pub fn refresh(&self, audit: &crate::audit::Audit, version: u64) -> Result<bool> {
        if Self::digest(&self.options, &self.limits)? == self.active.load().digest {
            return Ok(false);
        }
        let next = Self::load(&self.options, &self.limits)?;
        let operation = format!("reload-access-policy/{}", next.digest);
        audit.record("control", &operation, version, "attempt")?;
        self.active.store(Arc::new(next));
        if let Err(error) = audit.record("control", &operation, version, "committed") {
            log::error!("control reload audit: {error}");
        }
        Ok(true)
    }
}
pub struct Watch(pub Arc<crate::runtime::Shared>);
#[async_trait]
impl BackgroundService for Watch {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_error = String::new();
        loop {
            tokio::select! { _=shutdown.changed()=>break, _=tick.tick()=>{} }
            let controls = self.0.controls.clone();
            let audit = self.0.audit.clone();
            let version = self.0.snapshot.load().version;
            match tokio::task::spawn_blocking(move || controls.refresh(&audit, version)).await {
                Ok(Ok(changed)) => {
                    last_error.clear();
                    if changed {
                        self.0.telemetry.control_reloads.inc();
                    }
                }
                error => {
                    let message = format!("{error:?}");
                    if message != last_error {
                        self.0.telemetry.control_reload_errors.inc();
                        log::error!("management/tenant policy reload rejected: {message}");
                        last_error = message;
                    }
                }
            }
        }
    }
}
