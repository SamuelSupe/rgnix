use crate::runtime::Shared;
use async_trait::async_trait;
use futures::{StreamExt, stream};
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

pub struct Maintenance(pub Arc<Shared>);
#[async_trait]
impl BackgroundService for Maintenance {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let resolver = hickory_resolver::Resolver::builder_tokio()
            .map(|builder| builder.build())
            .map_err(|e| log::error!("DNS resolver initialization: {e}"))
            .ok();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! { _=shutdown.changed()=>break, _=tick.tick()=>{} }
            let snapshot = self.0.snapshot.load_full();
            self.0.telemetry.certificate_expiry.reset();
            self.0.telemetry.certificate_valid.reset();
            for host in snapshot.certificates.iter().take(2048) {
                let listener = host.listener.to_string();
                let labels = [listener.as_str(), host.name.as_str()];
                self.0
                    .telemetry
                    .certificate_valid
                    .with_label_values(&labels)
                    .set(i64::from(host.certificate.as_ref().is_some_and(|cert| {
                        cert.valid_time().is_ok() && cert.matches_name(&host.name)
                    })));
                if let Some(expires) = host
                    .certificate
                    .as_ref()
                    .and_then(|cert| cert.expires_at().ok())
                {
                    self.0
                        .telemetry
                        .certificate_expiry
                        .with_label_values(&labels)
                        .set(expires);
                }
            }
            let mut unique = BTreeSet::new();
            let backends: Vec<_> = snapshot
                .backends
                .values()
                .filter(|b| unique.insert(Arc::as_ptr(b) as usize))
                .cloned()
                .collect();
            let mut keys = BTreeSet::new();
            let verifiers: Vec<_> = snapshot
                .hosts
                .iter()
                .flat_map(|h| &h.routes)
                .filter_map(|r| r.settings.security.jwt.clone())
                .filter(|jwt| keys.insert(Arc::as_ptr(jwt) as usize))
                .collect();
            let mut refreshes = stream::iter(
                verifiers
                    .into_iter()
                    .map(|jwt| async move { jwt.refresh().await }),
            )
            .buffer_unordered(8);
            loop {
                tokio::select! {_=shutdown.changed()=>return,result=refreshes.next()=>if result.is_none(){break;}}
            }
            let mut checks = stream::iter(backends.into_iter().map(|backend| {
                let resolver = &resolver;
                async move {
                    backend.maintain(resolver.as_ref()).await;
                }
            }))
            .buffer_unordered(8);
            loop {
                tokio::select! { _=shutdown.changed()=>return, result=checks.next()=>if result.is_none() {break;} }
            }
        }
    }
}
