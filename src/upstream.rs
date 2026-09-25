use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum Protocol {
    #[default]
    Http1,
    Http2,
    Auto,
}
impl Protocol {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "1.1" => Ok(Self::Http1),
            "2" | "2.0" => Ok(Self::Http2),
            "auto" => Ok(Self::Auto),
            _ => anyhow::bail!("upstream HTTP version expects 1.1, 2 or auto"),
        }
    }
    pub fn versions(&self) -> (u8, u8) {
        match self {
            Self::Http1 => (1, 1),
            Self::Http2 => (2, 2),
            Self::Auto => (2, 1),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Transport {
    pub protocol: Protocol,
    pub server_name: Option<String>,
    pub ca_pem: Option<String>,
    #[serde(skip)]
    pub ca: Option<Arc<Box<[openssl::x509::X509]>>>,
    #[serde(skip)]
    pub identity: Option<Arc<pingora::utils::tls::CertKey>>,
    #[serde(skip)]
    pub identity_pem: Option<(String, String)>,
    pub identity_digest: Option<String>,
    #[serde(skip)]
    ca_digest: [u8; 32],
}
impl Transport {
    pub fn set_identity(&mut self, cert: String, key: String) -> Result<()> {
        ensure!(
            cert.len() + key.len() <= 1024 * 1024,
            "upstream identity exceeds 1 MiB"
        );
        let parsed = crate::model::Certificate::parse(cert.as_bytes(), key.as_bytes())?;
        let normalized_key = String::from_utf8(parsed.key.private_key_to_pem_pkcs8()?)?;
        let mut chain = vec![parsed.leaf];
        chain.extend(parsed.chain);
        self.identity = Some(Arc::new(pingora::utils::tls::CertKey::new(
            chain, parsed.key,
        )));
        use sha2::Digest;
        self.identity_digest = Some(format!(
            "{:x}",
            sha2::Sha256::digest(format!("{cert}{key}"))
        ));
        self.identity_pem = Some((cert, normalized_key));
        Ok(())
    }
    pub fn client_builder(&self) -> Result<reqwest::ClientBuilder> {
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = &self.ca_pem {
            builder = builder.tls_built_in_root_certs(false);
            for cert in reqwest::Certificate::from_pem_bundle(pem.as_bytes())? {
                builder = builder.add_root_certificate(cert);
            }
        }
        if let Some((cert, key)) = &self.identity_pem {
            builder = builder.identity(reqwest::Identity::from_pkcs8_pem(
                cert.as_bytes(),
                key.as_bytes(),
            )?);
        }
        Ok(builder)
    }
    pub fn set_ca(&mut self, pem: String) -> Result<()> {
        ensure!(pem.len() <= 1024 * 1024, "upstream CA exceeds 1 MiB");
        let certs = openssl::x509::X509::stack_from_pem(pem.as_bytes())?;
        ensure!(!certs.is_empty(), "empty upstream CA bundle");
        use sha2::Digest;
        self.ca_digest = sha2::Sha256::digest(pem.as_bytes()).into();
        self.ca = Some(Arc::new(certs.into_boxed_slice()));
        self.ca_pem = Some(pem);
        Ok(())
    }
    pub fn pool_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.ca_digest.hash(&mut hash);
        self.server_name.hash(&mut hash);
        self.identity_digest.hash(&mut hash);
        self.protocol.versions().hash(&mut hash);
        hash.finish()
    }
}
