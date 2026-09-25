use anyhow::{Result, ensure};
use openssl::{
    ssl::{SslRef, SslVerifyMode},
    stack::Stack,
    x509::{
        X509, X509StoreContext,
        store::{X509Store, X509StoreBuilder},
        verify::X509VerifyFlags,
    },
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub enum Mode {
    #[default]
    Off,
    Optional,
    Required,
}
#[derive(Clone, Default, Serialize)]
pub struct Policy {
    pub mode: Mode,
    pub ca_pem: Option<String>,
    #[serde(skip)]
    pub store: Option<Arc<X509Store>>,
}
impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientTls")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}
pub struct Peer {
    pub leaf: X509,
    pub chain: Stack<X509>,
}
impl Policy {
    pub fn set_ca(&mut self, pem: String) -> Result<()> {
        let store = store(&pem)?;
        self.ca_pem = Some(pem);
        self.store = Some(Arc::new(store));
        Ok(())
    }
    pub fn configure(&self, ssl: &mut SslRef) -> Result<()> {
        if self.mode == Mode::Off {
            ssl.set_verify(SslVerifyMode::NONE);
            return Ok(());
        }
        let pem = self
            .ca_pem
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("client TLS CA missing"))?;
        ssl.set_verify_cert_store(store(pem)?)?;
        ssl.set_verify(if self.mode == Mode::Required {
            SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT
        } else {
            SslVerifyMode::PEER
        });
        Ok(())
    }
    pub fn authorize(&self, peer: Option<&Peer>) -> bool {
        if self.mode == Mode::Off {
            return true;
        }
        let Some(peer) = peer else {
            return self.mode == Mode::Optional;
        };
        let Some(store) = &self.store else {
            return false;
        };
        let Ok(mut context) = X509StoreContext::new() else {
            return false;
        };
        context
            .init(store, &peer.leaf, &peer.chain, |c| c.verify_cert())
            .unwrap_or(false)
    }
}
fn store(pem: &str) -> Result<X509Store> {
    ensure!(pem.len() <= 1024 * 1024, "client CA exceeds 1 MiB");
    let certs = X509::stack_from_pem(pem.as_bytes())?;
    ensure!(
        !certs.is_empty() && certs.len() <= 64,
        "client CA requires 1..64 certificates"
    );
    let mut builder = X509StoreBuilder::new()?;
    builder.set_flags(X509VerifyFlags::TRUSTED_FIRST)?;
    builder.set_purpose(openssl::x509::X509PurposeId::SSL_CLIENT)?;
    for cert in certs {
        builder.add_cert(cert)?;
    }
    Ok(builder.build())
}
pub fn peer(ssl: &SslRef) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
    let leaf = ssl.peer_certificate()?;
    let mut chain = Stack::new().ok()?;
    if let Some(certs) = ssl.peer_cert_chain() {
        for cert in certs {
            chain.push(cert.to_owned()).ok()?;
        }
    }
    Some(Arc::new(Peer { leaf, chain }))
}
