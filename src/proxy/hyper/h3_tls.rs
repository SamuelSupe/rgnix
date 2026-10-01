use crate::{model::RuntimeSnapshot, security::mtls::Mode};
use anyhow::Result;
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

struct Certificates {
    snapshot: RuntimeSnapshot,
    address: SocketAddr,
    keys: HashMap<String, Arc<CertifiedKey>>,
}
impl std::fmt::Debug for Certificates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Certificates").finish_non_exhaustive()
    }
}
impl ResolvesServerCert for Certificates {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = self
            .snapshot
            .tls_host(self.address, hello.server_name().unwrap_or(""))?;
        host.certificate.as_ref()?.valid_time().ok()?;
        self.keys.get(&host.name).cloned()
    }
}

pub(super) fn config(
    snapshot: &RuntimeSnapshot,
    address: SocketAddr,
) -> Result<quinn::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut keys = HashMap::new();
    let hosts: Vec<_> = snapshot
        .certificates
        .iter()
        .filter(|c| c.listener == address)
        .collect();
    for host in &hosts {
        if let Some(certificate) = &host.certificate {
            let mut chain = vec![CertificateDer::from(certificate.leaf.to_der()?)];
            for cert in &certificate.chain {
                chain.push(CertificateDer::from(cert.to_der()?));
            }
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                certificate.key.private_key_to_pkcs8()?,
            ));
            let key = provider.key_provider.load_private_key(key)?;
            keys.insert(host.name.clone(), Arc::new(CertifiedKey::new(chain, key)));
        }
    }
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?;
    let authenticated: Vec<_> = hosts
        .iter()
        .filter(|h| h.client_auth.mode != Mode::Off)
        .collect();
    let builder = if !authenticated.is_empty() {
        let mut roots = rustls::RootCertStore::empty();
        for host in authenticated {
            if let Some(pem) = &host.client_auth.ca_pem {
                for cert in openssl::x509::X509::stack_from_pem(pem.as_bytes())? {
                    roots.add(CertificateDer::from(cert.to_der()?))?;
                }
            }
        }
        // QUIC selects its TLS configuration before SNI is decoded. Request a
        // certificate against the listener's CA union; routing revalidates the
        // peer against that host's current CA and required/optional policy.
        let verifier =
            rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider);
        builder.with_client_cert_verifier(verifier.allow_unauthenticated().build()?)
    } else {
        builder.with_no_client_auth()
    };
    let mut certificates = snapshot.clone();
    certificates.hyper = None;
    certificates.hosts.clear();
    certificates.backends.clear();
    certificates
        .certificates
        .retain(|host| host.listener == address);
    let mut tls = builder.with_cert_resolver(Arc::new(Certificates {
        snapshot: certificates,
        address,
        keys,
    }));
    // TLS state is reused only while certificate, SNI and client-CA policy match.
    // Credential changes invalidate previous tickets. Never allow replayable 0-RTT.
    tls.session_storage = rustls::server::ServerSessionMemoryCache::new(512);
    tls.send_tls13_tickets = 2;
    tls.max_early_data_size = 0;
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(128u32.into());
    transport.max_concurrent_uni_streams(16u32.into());
    transport.stream_receive_window((1024 * 1024u32).into());
    transport.receive_window((8 * 1024 * 1024u32).into());
    transport.max_idle_timeout(Some(std::time::Duration::from_secs(75).try_into()?));
    config.transport_config(Arc::new(transport));
    Ok(config)
}
