use crate::{
    config, ingress, model::RuntimeSnapshot, proxy::Proxy, script::Compiler, telemetry::Telemetry,
};
use anyhow::{Context, Result, ensure};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::FutureExt;
use pingora::{
    apps::http_app::ServeHttp,
    listeners::{TlsAccept, tls::TlsSettings},
    protocols::http::ServerSession,
    server::{Server, ShutdownWatch, configuration::ServerConf},
    services::{
        background::{BackgroundService, background_service},
        listening::Service,
    },
    tls::{ext, ssl::SslRef},
};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};

pub struct Shared {
    pub snapshot: ArcSwap<RuntimeSnapshot>,
    pub compiler: Arc<Compiler>,
    pub telemetry: Arc<Telemetry>,
    pub requests: Arc<tokio::sync::Semaphore>,
    pub plugins: Arc<tokio::sync::Semaphore>,
    pub upstream_max_fails: usize,
    pub upstream_fail_timeout: std::time::Duration,
}
#[derive(Clone, clap::Args)]
pub struct Limits {
    #[arg(long, default_value_t = 2)]
    pub threads: usize,
    #[arg(long, default_value_t = 1024)]
    pub max_inflight: usize,
    #[arg(long, default_value_t = 32)]
    pub max_plugin_instances: usize,
    #[arg(long, default_value_t = 3)]
    pub upstream_max_fails: usize,
    #[arg(long, default_value_t = 10)]
    pub upstream_fail_timeout_secs: u64,
}
pub enum Source {
    File(PathBuf),
    Ingress(ingress::Options),
}

impl Shared {
    pub fn publish(&self, mut snapshot: RuntimeSnapshot) -> Result<()> {
        let current = self.snapshot.load();
        ensure!(
            current.listeners == snapshot.listeners,
            "changing listener addresses or TLS/http2 options requires restart"
        );
        snapshot.version = current.version + 1;
        for (name, backend) in &mut snapshot.backends {
            if let Some(previous) = current.backends.get(name)
                && backend.endpoints == previous.endpoints
                && backend.tls == previous.tls
                && backend.hostname == previous.hostname
                && backend.host_header == previous.host_header
            {
                *backend = previous.clone();
            }
        }
        snapshot.content_hash = snapshot.fingerprint()?;
        snapshot.reindex();
        crate::logging::configure(snapshot.error_log.clone());
        self.telemetry.version.set(snapshot.version as i64);
        self.telemetry.config_info.reset();
        self.telemetry
            .config_info
            .with_label_values(&[&snapshot.content_hash])
            .set(1);
        let ready = snapshot.ready;
        self.snapshot.store(Arc::new(snapshot));
        self.telemetry.reloads.inc();
        self.telemetry.ready.store(ready, Ordering::Release);
        Ok(())
    }
}
struct Certificates {
    shared: Arc<Shared>,
    listener: SocketAddr,
}
#[async_trait]
impl TlsAccept for Certificates {
    async fn certificate_callback(&self, ssl: &mut SslRef) {
        let name = ssl
            .servername(openssl::ssl::NameType::HOST_NAME)
            .unwrap_or("");
        let snapshot = self.shared.snapshot.load();
        if let Some(cert) = snapshot.certificate(self.listener, name) {
            let result = (|| -> Result<()> {
                ext::ssl_use_certificate(ssl, &cert.leaf)?;
                ext::ssl_use_private_key(ssl, &cert.key)?;
                for chain in &cert.chain {
                    ext::ssl_add_chain_cert(ssl, chain)?;
                }
                Ok(())
            })();
            if let Err(e) = result {
                log::error!("TLS certificate callback: {e}");
            }
        }
    }
}
struct Admin(Arc<Telemetry>);
#[async_trait]
impl ServeHttp for Admin {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        let (status, body, content_type) = self.0.render(session.req_header().uri.path());
        session.set_keepalive(None);
        http::Response::builder()
            .status(status)
            .header("Content-Type", content_type)
            .header("Cache-Control", "no-store")
            .body(body)
            .unwrap()
    }
}
struct Control {
    shared: Arc<Shared>,
    source: Source,
}
#[async_trait]
impl BackgroundService for Control {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        match &self.source {
            Source::File(path) => {
                let mut hup =
                    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                        Ok(s) => s,
                        Err(e) => {
                            log::error!("SIGHUP listener: {e}");
                            return;
                        }
                    };
                loop {
                    tokio::select! {
                        _=shutdown.changed()=>break,
                        signal=hup.recv()=>{
                            if signal.is_none(){break;}
                            let path=path.clone();let compiler=self.shared.compiler.clone();
                            let result=tokio::task::spawn_blocking(move||config::load(&path,&compiler,0)).await;
                            let result=result.context("configuration task failed").and_then(|r|r).and_then(|s|self.shared.publish(s));
                            if let Err(e)=result {self.shared.telemetry.reload_errors.inc();log::error!("configuration rejected; keeping active snapshot: {e:#}");}else{log::info!("configuration reloaded");}
                        }
                    }
                }
            }
            Source::Ingress(options) => {
                let result = std::panic::AssertUnwindSafe(ingress::run(
                    self.shared.clone(),
                    options.clone(),
                    shutdown.clone(),
                ))
                .catch_unwind()
                .await;
                let result = result.unwrap_or_else(|_| Err(anyhow::anyhow!("controller panicked")));
                if let Err(e) = result {
                    log::error!("Ingress controller stopped: {e:#}");
                    self.shared.telemetry.ready.store(false, Ordering::Release);
                    self.shared
                        .telemetry
                        .healthy
                        .store(false, Ordering::Release);
                }
            }
        }
        self.shared.telemetry.ready.store(false, Ordering::Release);
    }
}

pub fn serve(
    mut snapshot: RuntimeSnapshot,
    compiler: Arc<Compiler>,
    source: Source,
    admin: SocketAddr,
    limits: Limits,
) -> Result<()> {
    let threads = limits.threads;
    ensure!(
        limits.upstream_max_fails <= 1000
            && (1..=3600).contains(&limits.upstream_fail_timeout_secs),
        "upstream-max-fails must be 0..1000 and upstream-fail-timeout-secs 1..3600"
    );
    ensure!(threads > 0 && threads <= 256, "threads must be 1..256");
    ensure!(
        limits.max_inflight > 0 && limits.max_inflight <= 1_000_000,
        "max-inflight must be 1..1000000"
    );
    ensure!(
        limits.max_plugin_instances > 0 && limits.max_plugin_instances <= limits.max_inflight,
        "max-plugin-instances must be 1..max-inflight"
    );
    snapshot.content_hash = snapshot.fingerprint()?;
    snapshot.reindex();
    crate::logging::configure(snapshot.error_log.clone());
    let telemetry = Telemetry::new()?;
    telemetry.version.set(snapshot.version as i64);
    telemetry
        .config_info
        .with_label_values(&[&snapshot.content_hash])
        .set(1);
    telemetry
        .ready
        .store(matches!(source, Source::File(_)), Ordering::Release);
    let listeners = snapshot.listeners.clone();
    let shared = Arc::new(Shared {
        snapshot: ArcSwap::from_pointee(snapshot),
        compiler,
        telemetry,
        requests: Arc::new(tokio::sync::Semaphore::new(limits.max_inflight)),
        plugins: Arc::new(tokio::sync::Semaphore::new(limits.max_plugin_instances)),
        upstream_max_fails: limits.upstream_max_fails,
        upstream_fail_timeout: std::time::Duration::from_secs(limits.upstream_fail_timeout_secs),
    });
    let conf = ServerConf {
        threads,
        daemon: false,
        // Pingora includes the first attempt in this budget.
        max_retries: 1,
        grace_period_seconds: Some(5),
        graceful_shutdown_timeout_seconds: Some(25),
        max_blocking_threads: Some(16),
        ..ServerConf::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, conf);
    server.bootstrap();
    for listener in listeners {
        let mut service = pingora::proxy::http_proxy_service(
            &server.configuration,
            Proxy {
                shared: shared.clone(),
                listener: listener.address,
                tls: listener.tls,
            },
        );
        if listener.tls {
            let mut tls = TlsSettings::with_callbacks(Box::new(Certificates {
                shared: shared.clone(),
                listener: listener.address,
            }))?;
            tls.set_min_proto_version(Some(openssl::ssl::SslVersion::TLS1_2))?;
            if listener.http2 {
                tls.enable_h2();
            }
            service.add_tls_with_settings(&listener.address.to_string(), None, tls);
        } else {
            service.add_tcp(&listener.address.to_string());
        }
        server.add_service(service);
    }
    let mut admin_service = Service::new("admin".into(), Admin(shared.telemetry.clone()));
    admin_service.add_tcp(&admin.to_string());
    admin_service.threads = Some(1);
    server.add_service(admin_service);
    server.add_service(background_service("control", Control { shared, source }));
    server.run_forever()
}
