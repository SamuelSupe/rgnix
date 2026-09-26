use crate::{
    config, ingress, model::RuntimeSnapshot, proxy::Proxy, script::Compiler, telemetry::Telemetry,
};
mod drain;
use anyhow::{Context, Result, ensure};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::FutureExt;
use pingora::{
    listeners::{TlsAccept, tls::TlsSettings},
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
    sync::{Arc, Mutex, atomic::Ordering},
};

pub struct Shared {
    pub(crate) fleet: crate::fleet::State,
    pub audit: Arc<crate::audit::Audit>,
    pub snapshot: ArcSwap<RuntimeSnapshot>,
    pub compiler: Arc<Compiler>,
    pub telemetry: Arc<Telemetry>,
    pub requests: Arc<tokio::sync::Semaphore>,
    pub plugins: Arc<tokio::sync::Semaphore>,
    pub traffic: Arc<crate::traffic::Limiter>,
    pub tenants: crate::tenancy::Tenants,
    pub rollouts: crate::rollout::Rollouts,
    pub mirrors: Arc<tokio::sync::Semaphore>,
    pub simulations: Arc<tokio::sync::Semaphore>,
    pub controls: Arc<crate::controls::Controls>,
    pub metric_results:
        Mutex<std::collections::BTreeMap<String, crate::rollout::metrics::Observation>>,
    publication: Mutex<Publication>,
    pub file_mode: bool,
    pub(crate) ingress_preview: Mutex<Option<crate::ingress::PreviewInput>>,
    pub(crate) gateway_preview: Mutex<Option<crate::gateway::PreviewInput>>,
    pub auth_client: reqwest::Client,
    pub upstream_max_fails: usize,
    pub upstream_fail_timeout: std::time::Duration,
}
#[derive(Clone, clap::Args)]
pub struct Limits {
    /// Optional administrator-owned marker file; creation starts connection draining.
    #[arg(long)]
    pub drain_file: Option<PathBuf>,
    #[arg(long, default_value_t = 5)]
    pub shutdown_grace_seconds: u64,
    #[arg(long, default_value_t = 25)]
    pub shutdown_timeout_seconds: u64,
    /// Publish per-Pod configuration acknowledgements and observe peers selected by publish-service.
    #[arg(long)]
    pub report_replicas: bool,
    /// Administrator-controlled namespace quotas and domains (JSON); automatically reloaded.
    #[arg(long)]
    pub tenant_policy_file: Option<PathBuf>,
    /// Administrator-owned HTTP metric providers for release gates (JSON).
    #[arg(long)]
    pub rollout_metrics_file: Option<PathBuf>,
    /// Optional administrator Redis coordinator for shared route and namespace request rates.
    #[arg(long)]
    pub global_rate_limit_file: Option<PathBuf>,
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
    Gateway(crate::gateway::Options),
}

#[derive(Default)]
struct Publication {
    previous: std::collections::VecDeque<Arc<RuntimeSnapshot>>,
    failures: std::collections::VecDeque<String>,
    durable: Option<crate::history::History>,
}
impl Shared {
    pub(crate) fn preview(&self) -> Self {
        Self {
            fleet: Default::default(),
            audit: self.audit.clone(),
            snapshot: ArcSwap::from(self.snapshot.load_full()),
            compiler: self.compiler.clone(),
            telemetry: self.telemetry.clone(),
            requests: Arc::new(tokio::sync::Semaphore::new(1)),
            plugins: Arc::new(tokio::sync::Semaphore::new(1)),
            traffic: Default::default(),
            tenants: Default::default(),
            rollouts: Default::default(),
            mirrors: Arc::new(tokio::sync::Semaphore::new(1)),
            simulations: Arc::new(tokio::sync::Semaphore::new(1)),
            controls: self.controls.clone(),
            metric_results: Mutex::new(Default::default()),
            publication: Mutex::new(Publication::default()),
            file_mode: self.file_mode,
            ingress_preview: Mutex::new(None),
            gateway_preview: Mutex::new(None),
            auth_client: self.auth_client.clone(),
            upstream_max_fails: self.upstream_max_fails,
            upstream_fail_timeout: self.upstream_fail_timeout,
        }
    }
    pub fn history(&self) -> serde_json::Value {
        let publication = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.snapshot.load_full();
        serde_json::json!({"current":current.version,"rollback":if self.file_mode {"file snapshots"} else {"change the Kubernetes source resources"},"versions":publication.previous.iter().chain(std::iter::once(&current)).map(|s|serde_json::json!({"version":s.version,"sha256":s.content_hash,"ready":s.ready})).collect::<Vec<_>>(),"durable_versions":publication.durable.as_ref().map(|d|d.versions()),"recent_errors":publication.failures})
    }
    pub fn rejected_update(&self, error: &str) {
        let mut publication = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        publication
            .failures
            .push_back(error.chars().take(2048).collect());
        while publication.failures.len() > 16 {
            publication.failures.pop_front();
        }
    }
    pub fn rollback(&self, version: u64) -> Result<()> {
        ensure!(
            self.file_mode,
            "Ingress versions are managed by Kubernetes; update source resources to preserve live withdrawals"
        );
        let mut publication = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = if let Some(durable) = &publication.durable {
            durable.restore(&self.compiler, version)?
        } else {
            (**publication
                .previous
                .iter()
                .find(|s| s.version == version)
                .context("version is not retained")?)
            .clone()
        };
        self.publish_locked(snapshot, &mut publication)
    }

    pub fn publish(&self, snapshot: RuntimeSnapshot) -> Result<()> {
        let mut publication = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        self.publish_locked(snapshot, &mut publication)
    }
    fn publish_locked(
        &self,
        mut snapshot: RuntimeSnapshot,
        publication: &mut Publication,
    ) -> Result<()> {
        let current = self.snapshot.load_full();
        ensure!(
            current.listeners == snapshot.listeners,
            "changing listener addresses or TLS/http2 options requires restart"
        );
        snapshot.version = current.version + 1;
        for (name, backend) in &mut snapshot.backends {
            if let Some(previous) = current.backends.get(name)
                && backend.same_config(previous)
            {
                *backend = previous.clone();
            }
        }
        snapshot.content_hash = snapshot.fingerprint()?;
        snapshot.reindex();
        self.audit
            .record("control", "publish", snapshot.version, "validated")?;
        if let Some(durable) = &mut publication.durable {
            durable.record(&snapshot)?;
        }
        crate::logging::configure(snapshot.error_log.clone());
        self.telemetry
            .files
            .configure(snapshot.log_rotation.clone());
        self.telemetry.version.set(snapshot.version as i64);
        self.telemetry.config_info.reset();
        self.telemetry
            .config_info
            .with_label_values(&[&snapshot.content_hash])
            .set(1);
        let ready = snapshot.ready;
        publication.previous.push_back(current);
        while publication.previous.len() > 8 {
            publication.previous.pop_front();
        }
        self.snapshot.store(Arc::new(snapshot));
        self.telemetry.reloads.inc();
        self.telemetry
            .config_updated
            .set(k8s_openapi::chrono::Utc::now().timestamp() as f64);
        if let Err(e) = self.audit.record(
            "control",
            "publish",
            self.snapshot.load().version,
            "committed",
        ) {
            log::error!("publication audit: {e}");
        }
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
        if let Some(host) = snapshot.tls_host(self.listener, name)
            && let Some(cert) = &host.certificate
            && cert.valid_time().is_ok()
        {
            let result = (|| -> Result<()> {
                host.client_auth.configure(ssl)?;
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
    async fn handshake_complete_callback(
        &self,
        ssl: &SslRef,
    ) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        crate::security::mtls::peer(ssl)
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
                            let state=self.shared.clone();
                            let started=std::time::Instant::now();
                            let result=tokio::task::spawn_blocking(move||state.publish(std::sync::Arc::new(config::bundle::Bundle::capture(&path)?).compile(&compiler,0)?)).await;
                            let result=result.context("configuration task failed").and_then(|r|r);
                            self.shared.telemetry.config_update_seconds.with_label_values(&["file",if result.is_ok(){"success"}else{"error"}]).observe(started.elapsed().as_secs_f64());
                            if let Err(e)=result {self.shared.rejected_update(&format!("{e:#}"));self.shared.telemetry.reload_errors.inc();log::error!("configuration rejected; keeping active snapshot: {e:#}");}else{log::info!("configuration reloaded");}
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
            Source::Gateway(options) => {
                let result = std::panic::AssertUnwindSafe(crate::gateway::run(
                    self.shared.clone(),
                    options.clone(),
                    shutdown.clone(),
                ))
                .catch_unwind()
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    log::error!("Gateway controller stopped: {result:?}");
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
    otlp: crate::otlp::Options,
    diagnostics: crate::diagnostics::Options,
) -> Result<()> {
    ensure!(
        !limits.report_replicas || !matches!(source, Source::File(_)),
        "report-replicas requires Ingress or Gateway mode"
    );
    ensure!(
        diagnostics.admission_listen.is_some() == diagnostics.admission_tls_cert.is_some()
            && diagnostics.admission_listen.is_some() == diagnostics.admission_tls_key.is_some(),
        "admission-listen requires admission-tls-cert and admission-tls-key"
    );
    ensure!(
        diagnostics.admission_listen.is_none() || !matches!(source, Source::File(_)),
        "admission requires Ingress or Gateway mode"
    );
    let controls = Arc::new(crate::controls::Controls::new(
        diagnostics.clone(),
        limits.clone(),
    )?);
    ensure!(
        diagnostics.history_dir.is_none() || matches!(source, Source::File(_)),
        "history-dir is only supported in standalone mode"
    );
    let audit = Arc::new(crate::audit::Audit::open(
        diagnostics
            .admin_audit_file
            .as_deref()
            .unwrap_or(std::path::Path::new("/dev/stderr")),
    )?);
    let threads = limits.threads;
    ensure!(
        limits.upstream_max_fails <= 1000
            && (1..=3600).contains(&limits.upstream_fail_timeout_secs),
        "upstream-max-fails must be 0..1000 and upstream-fail-timeout-secs 1..3600"
    );
    ensure!(threads > 0 && threads <= 256, "threads must be 1..256");
    ensure!(
        limits.shutdown_grace_seconds <= 3600
            && (1..=86400).contains(&limits.shutdown_timeout_seconds),
        "invalid shutdown grace or timeout"
    );
    ensure!(
        limits.drain_file.as_ref().is_none_or(|p| !p.exists()),
        "drain marker exists at startup; remove it before starting a new process"
    );
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
    let telemetry = Telemetry::new(otlp)?;
    crate::logging::install_files(telemetry.files.clone());
    crate::logging::configure(snapshot.error_log.clone());
    telemetry.files.configure(snapshot.log_rotation.clone());
    telemetry.version.set(snapshot.version as i64);
    telemetry
        .config_info
        .with_label_values(&[&snapshot.content_hash])
        .set(1);
    telemetry
        .ready
        .store(matches!(source, Source::File(_)), Ordering::Release);
    let listeners = snapshot.listeners.clone();
    let file_mode = matches!(source, Source::File(_));
    if file_mode {
        telemetry
            .config_updated
            .set(k8s_openapi::chrono::Utc::now().timestamp() as f64);
    }
    let mut publication = Publication::default();
    if let (Some(directory), Source::File(path)) = (&diagnostics.history_dir, &source) {
        let mut durable = crate::history::History::open(directory, path)?;
        durable.record(&snapshot)?;
        publication.durable = Some(durable);
    }
    let shared = Arc::new(Shared {
        fleet: crate::fleet::State::new(limits.report_replicas),
        audit: audit.clone(),
        snapshot: ArcSwap::from_pointee(snapshot),
        compiler,
        telemetry: telemetry.clone(),
        requests: Arc::new(tokio::sync::Semaphore::new(limits.max_inflight)),
        plugins: Arc::new(tokio::sync::Semaphore::new(limits.max_plugin_instances)),
        traffic: Arc::new(crate::traffic::Limiter::default()),
        tenants: Default::default(),
        rollouts: Default::default(),
        mirrors: Arc::new(tokio::sync::Semaphore::new(16)),
        simulations: Arc::new(tokio::sync::Semaphore::new(2)),
        controls,
        metric_results: Mutex::new(Default::default()),
        publication: Mutex::new(publication),
        file_mode,
        ingress_preview: Mutex::new(None),
        gateway_preview: Mutex::new(None),
        auth_client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(3))
            .timeout(std::time::Duration::from_secs(5))
            .pool_max_idle_per_host(8)
            .build()?,
        upstream_max_fails: limits.upstream_max_fails,
        upstream_fail_timeout: std::time::Duration::from_secs(limits.upstream_fail_timeout_secs),
    });
    Telemetry::observe_runtime(&shared, &limits)?;
    let conf = ServerConf {
        threads,
        daemon: false,
        // Pingora includes the first attempt in this budget.
        max_retries: 1,
        grace_period_seconds: Some(limits.shutdown_grace_seconds),
        graceful_shutdown_timeout_seconds: Some(limits.shutdown_timeout_seconds),
        max_blocking_threads: Some(16),
        ..ServerConf::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, conf);
    let draining = shared.clone();
    server.set_graceful_shutdown_check(move || {
        draining.requests.available_permits() == limits.max_inflight
    });
    server.add_service(background_service(
        "connection-drain",
        drain::Drain {
            marker: limits.drain_file.clone(),
            telemetry: telemetry.clone(),
        },
    ));
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
        if !listener.tls && listener.http2 {
            let mut options = pingora::apps::HttpServerOptions::default();
            options.h2c = true;
            service.app_logic_mut().unwrap().server_options = Some(options);
        }
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
        if listener.proxy_protocol {
            service.endpoints().set_pre_tls_callback(Arc::new(
                crate::proxy_protocol::ProxyProtocol {
                    trusted: listener.proxy_trusted.clone(),
                },
            ));
        }
        server.add_service(service);
    }
    let mut admin_service = Service::new(
        "admin".into(),
        crate::diagnostics::Admin {
            shared: shared.clone(),
            audit,
        },
    );
    admin_service.add_tcp(&admin.to_string());
    admin_service.threads = Some(1);
    server.add_service(admin_service);
    if let Some(address) = diagnostics.admission_listen {
        let mut admission = Service::new(
            "admission".into(),
            crate::admission::Admission {
                shared: shared.clone(),
                controller_user: diagnostics.admission_controller_user.clone(),
            },
        );
        let certificate = crate::admission::tls::ReloadingCertificate::new(
            diagnostics.admission_tls_cert.clone().unwrap(),
            diagnostics.admission_tls_key.clone().unwrap(),
            &shared.telemetry.registry,
        )?;
        admission.add_tls_with_settings(
            &address.to_string(),
            None,
            TlsSettings::with_callbacks(Box::new(certificate.clone()))?,
        );
        server.add_service(background_service("admission-certificates", certificate));
        server.add_service(admission);
    }
    server.add_service(background_service(
        "upstream-maintenance",
        crate::maintenance::Maintenance(shared.clone()),
    ));
    server.add_service(background_service(
        "access-policy-watch",
        crate::controls::Watch(shared.clone()),
    ));
    server.add_service(background_service(
        "rollout-metrics",
        crate::rollout::metrics::Poller(shared.clone()),
    ));
    server.add_service(background_service("control", Control { shared, source }));
    server.run(pingora::server::RunArgs::default());
    if let Some(exporter) = &telemetry.otlp {
        exporter.shutdown();
    }
    if let Some(exporter) = &telemetry.traces {
        exporter.shutdown();
    }
    telemetry.files.shutdown();
    Ok(())
}
