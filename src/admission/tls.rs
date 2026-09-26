use crate::model::Certificate;
use anyhow::{Result, ensure};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::{
    listeners::TlsAccept,
    server::ShutdownWatch,
    services::background::BackgroundService,
    tls::{ext, ssl::SslRef},
};
use prometheus::{IntCounter, IntGauge, Registry};
use sha2::{Digest, Sha256};
use std::{io::Read, path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone)]
pub(crate) struct ReloadingCertificate {
    cert: PathBuf,
    key: PathBuf,
    active: Arc<ArcSwap<Certificate>>,
    expiry: IntGauge,
    valid: IntGauge,
    failures: IntCounter,
}
impl ReloadingCertificate {
    pub fn new(cert: PathBuf, key: PathBuf, registry: &Registry) -> Result<Self> {
        let (_, active) = Self::read(&cert, &key)?;
        let expiry = IntGauge::new(
            "rgnix_admission_certificate_expiry_timestamp_seconds",
            "Admission TLS certificate expiration",
        )?;
        let valid = IntGauge::new(
            "rgnix_admission_certificate_valid",
            "Admission TLS certificate is currently valid",
        )?;
        let failures = IntCounter::new(
            "rgnix_admission_certificate_reload_errors_total",
            "Rejected admission TLS certificate updates",
        )?;
        registry.register(Box::new(expiry.clone()))?;
        registry.register(Box::new(valid.clone()))?;
        registry.register(Box::new(failures.clone()))?;
        Ok(Self {
            cert,
            key,
            active: Arc::new(ArcSwap::from_pointee(active)),
            expiry,
            valid,
            failures,
        })
    }
    fn read(cert: &std::path::Path, key: &std::path::Path) -> Result<([u8; 32], Certificate)> {
        fn read(path: &std::path::Path) -> Result<Vec<u8>> {
            let mut data = Vec::new();
            std::fs::File::open(path)?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut data)?;
            ensure!(
                data.len() <= 1024 * 1024,
                "admission TLS file exceeds 1 MiB"
            );
            Ok(data)
        }
        let cert = read(cert)?;
        let key = read(key)?;
        let mut hash = Sha256::new();
        hash.update(&cert);
        hash.update(&key);
        Ok((hash.finalize().into(), Certificate::parse(&cert, &key)?))
    }
}
#[async_trait]
impl TlsAccept for ReloadingCertificate {
    async fn certificate_callback(&self, ssl: &mut SslRef) {
        let cert = self.active.load();
        let result = (|| -> Result<()> {
            cert.valid_time()?;
            ext::ssl_use_certificate(ssl, &cert.leaf)?;
            ext::ssl_use_private_key(ssl, &cert.key)?;
            for chain in &cert.chain {
                ext::ssl_add_chain_cert(ssl, chain)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            log::error!("admission TLS: {error}");
        }
    }
}
#[async_trait]
impl BackgroundService for ReloadingCertificate {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut digest = None;
        let mut last_error = String::new();
        loop {
            tokio::select! { _ = shutdown.changed() => break, _ = tick.tick() => {} }
            match ReloadingCertificate::read(&self.cert, &self.key) {
                Ok((next, certificate)) => {
                    if digest != Some(next) {
                        self.active.store(Arc::new(certificate));
                        digest = Some(next);
                    }
                    last_error.clear();
                }
                Err(error) => {
                    let error = error.to_string();
                    if last_error != error {
                        self.failures.inc();
                        log::error!(
                            "admission TLS update rejected, retaining last valid certificate: {error}"
                        );
                        last_error = error;
                    }
                }
            }
            let cert = self.active.load();
            self.expiry.set(cert.expires_at().unwrap_or(0));
            self.valid.set(i64::from(cert.valid_time().is_ok()));
        }
    }
}
