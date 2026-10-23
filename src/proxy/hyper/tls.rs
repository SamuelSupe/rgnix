use crate::{model::Listener as ListenerConfig, upstream::Transport};
use anyhow::{Result, ensure};
use openssl::ssl::{
    AlpnError, NameType, SniError, Ssl, SslAcceptor, SslConnector, SslMethod, SslRef, SslVersion,
};
use std::{net::SocketAddr, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

pub(super) fn acceptor(
    snapshot: &crate::model::RuntimeSnapshot,
    listener: &ListenerConfig,
) -> Result<SslAcceptor> {
    let mut certificates = snapshot.clone();
    certificates.hyper = None;
    certificates.hosts.clear();
    certificates.backends.clear();
    certificates
        .certificates
        .retain(|host| host.listener == listener.address);
    let certificates = Arc::new(certificates);
    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())?;
    builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    let address = listener.address;
    // Selection happens for each handshake, including after Secret removal.
    // A listener with no current certificate must fail closed.
    builder.set_servername_callback(move |ssl, _| {
        install_certificate(ssl, &certificates, address).map_err(|_| SniError::ALERT_FATAL)
    });
    let hosts: Vec<_> = snapshot
        .certificates
        .iter()
        .filter(|c| c.listener == address)
        .collect();
    if hosts.len() == 1 && hosts[0].client_auth.mode == crate::security::mtls::Mode::Off {
        // Each publication owns new ticket keys and an isolated cache. Withdrawn
        // certificates cannot authenticate new connections through an old session.
        let mut session_id = [0; 32];
        openssl::rand::rand_bytes(&mut session_id)?;
        builder.set_session_id_context(&session_id)?;
        builder.set_session_cache_mode(openssl::ssl::SslSessionCacheMode::SERVER);
    } else {
        builder.set_session_cache_mode(openssl::ssl::SslSessionCacheMode::OFF);
        builder.set_options(openssl::ssl::SslOptions::NO_TICKET);
        // NO_TICKET alone still allows stateful TLS 1.3 tickets.
        builder.set_num_tickets(0)?;
    }
    let protocols: &'static [u8] = if listener.http2 {
        b"\x02h2\x08http/1.1"
    } else {
        b"\x08http/1.1"
    };
    builder.set_alpn_select_callback(move |_, offered| {
        openssl::ssl::select_next_proto(protocols, offered).ok_or(AlpnError::NOACK)
    });
    Ok(builder.build())
}

fn install_certificate(
    ssl: &mut SslRef,
    snapshot: &crate::model::RuntimeSnapshot,
    address: SocketAddr,
) -> Result<()> {
    let name = ssl.servername(NameType::HOST_NAME).unwrap_or("").to_owned();
    let host = snapshot
        .tls_host(address, &name)
        .ok_or_else(|| anyhow::anyhow!("unknown TLS host"))?;
    let cert = host
        .certificate
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("TLS certificate withdrawn"))?;
    cert.valid_time()?;
    host.client_auth.configure(ssl)?;
    ssl.set_certificate(&cert.leaf)?;
    ssl.set_private_key(&cert.key)?;
    for certificate in &cert.chain {
        ssl.add_chain_cert(certificate.clone())?;
    }
    Ok(())
}

pub(super) async fn accept(
    acceptor: &SslAcceptor,
    socket: TcpStream,
) -> Result<tokio_openssl::SslStream<TcpStream>> {
    let ssl = Ssl::new(acceptor.context())?;
    let mut stream = tokio_openssl::SslStream::new(ssl, socket)?;
    let handshake = futures::future::poll_fn(|cx| {
        clear_errors();
        Pin::new(&mut stream).poll_accept(cx)
    });
    tokio::time::timeout(Duration::from_secs(10), handshake).await??;
    Ok(stream)
}

pub(super) fn connector(transport: &Transport) -> Result<SslConnector> {
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    if let Some(certs) = &transport.ca {
        let mut store = openssl::x509::store::X509StoreBuilder::new()?;
        for certificate in certs.iter() {
            store.add_cert(certificate.clone())?;
        }
        builder.set_cert_store(store.build());
    }
    if let Some((cert, key)) = &transport.identity_pem {
        let mut chain = openssl::x509::X509::stack_from_pem(cert.as_bytes())?.into_iter();
        let leaf = chain
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty client certificate"))?;
        builder.set_certificate(&leaf)?;
        for cert in chain {
            builder.add_extra_chain_cert(cert)?;
        }
        let key = openssl::pkey::PKey::private_key_from_pem(key.as_bytes())?;
        builder.set_private_key(&key)?;
        builder.check_private_key()?;
    }
    builder.set_alpn_protos(match transport.protocol {
        crate::upstream::Protocol::Http1 => b"\x08http/1.1",
        crate::upstream::Protocol::Http2 => b"\x02h2",
        crate::upstream::Protocol::Auto => b"\x02h2\x08http/1.1",
    })?;
    Ok(builder.build())
}

pub(super) async fn connect(
    connector: &SslConnector,
    name: &str,
    socket: TcpStream,
    require_h2: bool,
) -> Result<Stream> {
    let ssl = connector.configure()?.into_ssl(name)?;
    let mut stream = tokio_openssl::SslStream::new(ssl, socket)?;
    futures::future::poll_fn(|cx| {
        clear_errors();
        Pin::new(&mut stream).poll_connect(cx)
    })
    .await?;
    ensure!(
        !require_h2 || stream.ssl().selected_alpn_protocol() == Some(b"h2"),
        "upstream did not negotiate required HTTP/2"
    );
    Ok(Stream::Tls(Box::new(stream)))
}

pub(super) enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_openssl::SslStream<TcpStream>>),
}

fn clear_errors() {
    // OpenSSL <= 3.x consults the thread-local error queue in SSL_get_error.
    // Other tasks can leave crypto errors between polls of the same TLS stream.
    drop(openssl::error::ErrorStack::get());
}
impl Stream {
    pub fn h2(&self) -> bool {
        matches!(self, Self::Tls(s) if s.ssl().selected_alpn_protocol() == Some(b"h2"))
    }
}
impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => {
                clear_errors();
                Pin::new(s.as_mut()).poll_read(cx, buf)
            }
        }
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => {
                clear_errors();
                Pin::new(s.as_mut()).poll_write(cx, buf)
            }
        }
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Self::Tls(s) => {
                clear_errors();
                Pin::new(s.as_mut()).poll_write_vectored(cx, bufs)
            }
        }
    }
    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Plain(s) => s.is_write_vectored(),
            Self::Tls(s) => s.is_write_vectored(),
        }
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => {
                clear_errors();
                Pin::new(s.as_mut()).poll_flush(cx)
            }
        }
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => {
                clear_errors();
                Pin::new(s.as_mut()).poll_shutdown(cx)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};

    #[tokio::test]
    async fn unrelated_crypto_error_does_not_close_a_waiting_tls_connection() {
        let key =
            openssl::pkey::PKey::from_rsa(openssl::rsa::Rsa::generate(2048).unwrap()).unwrap();
        let mut name = openssl::x509::X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "localhost").unwrap();
        let name = name.build();
        let mut certificate = openssl::x509::X509::builder().unwrap();
        certificate.set_version(2).unwrap();
        certificate.set_subject_name(&name).unwrap();
        certificate.set_issuer_name(&name).unwrap();
        certificate.set_pubkey(&key).unwrap();
        certificate
            .set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        certificate
            .set_not_after(&openssl::asn1::Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        certificate
            .sign(&key, openssl::hash::MessageDigest::sha256())
            .unwrap();
        let certificate = certificate.build();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        acceptor.set_certificate(&certificate).unwrap();
        acceptor.set_private_key(&key).unwrap();
        let acceptor = acceptor.build();
        let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(openssl::ssl::SslVerifyMode::NONE);
        let connector = connector.build();
        let (server, client) = tokio::join!(
            accept(&acceptor, socket),
            connect(&connector, "localhost", client, false)
        );
        let mut stream = Stream::Tls(Box::new(server.unwrap()));
        let client = client.unwrap();
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut buffer = [0; 1];
        let mut read = ReadBuf::new(&mut buffer);
        assert!(
            Pin::new(&mut stream)
                .poll_read(&mut cx, &mut read)
                .is_pending()
        );

        let errors = openssl::x509::X509::from_pem(b"invalid certificate").unwrap_err();
        errors.put();
        let result = Pin::new(&mut stream).poll_read(&mut cx, &mut read);
        assert!(matches!(result, Poll::Pending), "{result:?}");
        drop(client);
    }
}
