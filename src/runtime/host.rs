use super::{Limits, Shared};
#[cfg(feature = "hyper-experimental")]
use anyhow::Context;
use anyhow::Result;
use pingora::{
    server::{Server, configuration::ServerConf},
    services::ServiceWithDependents,
};
use std::sync::Arc;
#[cfg(feature = "hyper-experimental")]
use std::time::Duration;

pub(super) struct Host {
    pub configuration: Arc<ServerConf>,
    engine: Engine,
}

enum Engine {
    Pingora(Server),
    #[cfg(feature = "hyper-experimental")]
    Native {
        services: Vec<Box<dyn ServiceWithDependents>>,
        shared: Arc<Shared>,
        limits: Limits,
    },
}

impl Host {
    pub fn new(conf: ServerConf, shared: Arc<Shared>, limits: &Limits) -> Self {
        #[cfg(feature = "hyper-experimental")]
        if shared.experimental_hyper {
            return Self {
                configuration: Arc::new(conf),
                engine: Engine::Native {
                    services: Vec::new(),
                    shared,
                    limits: limits.clone(),
                },
            };
        }
        let mut server = Server::new_with_opt_and_conf(None, conf);
        let max = limits.max_inflight;
        server.set_graceful_shutdown_check(move || shared.requests.available_permits() == max);
        Self {
            configuration: server.configuration.clone(),
            engine: Engine::Pingora(server),
        }
    }

    pub fn add_service(&mut self, service: impl ServiceWithDependents + 'static) {
        match &mut self.engine {
            Engine::Pingora(server) => {
                server.add_service(service);
            }
            #[cfg(feature = "hyper-experimental")]
            Engine::Native { services, .. } => services.push(Box::new(service)),
        }
    }

    pub fn bootstrap(&mut self) {
        match &mut self.engine {
            Engine::Pingora(server) => server.bootstrap(),
            #[cfg(feature = "hyper-experimental")]
            Engine::Native { .. } => {}
        }
    }

    pub fn run(self) -> Result<()> {
        match self.engine {
            Engine::Pingora(server) => {
                server.run(Default::default());
                Ok(())
            }
            #[cfg(feature = "hyper-experimental")]
            Engine::Native {
                services,
                shared,
                limits,
            } => run(services, shared, limits),
        }
    }
}

#[cfg(feature = "hyper-experimental")]
fn run(
    services: Vec<Box<dyn ServiceWithDependents>>,
    shared: Arc<Shared>,
    limits: Limits,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime
        .block_on(async move {
            #[cfg(unix)]
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            #[cfg(unix)]
            let mut quit = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())?;
            #[cfg(unix)]
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            #[cfg(unix)]
            let _hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
            let (shutdown, watch) = tokio::sync::watch::channel(false);
            let (finish, finished) = tokio::sync::watch::channel(false);
            let stopped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut workers = Vec::new();
            shared.telemetry.data_plane.enable(Duration::from_secs(
                limits.hyper_worker_stall_timeout_seconds,
            ));
            let heartbeats: Vec<_> = services
                .iter()
                .map(|s| shared.telemetry.data_plane.register(s.name().to_owned()))
                .collect();
            for (mut service, heartbeat) in services.into_iter().zip(heartbeats) {
                let name = service.name().to_owned();
                let threads = service.threads().unwrap_or(limits.threads);
                let watch = watch.clone();
                let mut finished = finished.clone();
                let stopped = stopped.clone();
                let worker =
                    std::thread::Builder::new()
                        .name(name)
                        .spawn(move || -> Result<()> {
                            let mut builder = if threads == 1 {
                                tokio::runtime::Builder::new_current_thread()
                            } else {
                                let mut builder = tokio::runtime::Builder::new_multi_thread();
                                builder.worker_threads(threads);
                                builder
                            };
                            let runtime = builder
                                .enable_all()
                                .max_blocking_threads((16 / threads).clamp(1, 2))
                                .build()?;
                            let (ready, _) = tokio::sync::watch::channel(false);
                            runtime.block_on(async {
                                let heartbeat = tokio::spawn(heartbeat.run());
                                service
                                    .start_service(
                                        #[cfg(unix)]
                                        None,
                                        watch,
                                        1,
                                        pingora::services::ServiceReadyNotifier::new(ready),
                                    )
                                    .await;
                                heartbeat.abort();
                            });
                            stopped.fetch_add(1, std::sync::atomic::Ordering::Release);
                            // A shared H2 client can be driven by a reactor whose
                            // listener has drained before another listener's RPCs.
                            // Keep all reactors alive until every service drains.
                            runtime.block_on(async {
                                let _ = finished.changed().await;
                            });
                            runtime.shutdown_timeout(Duration::from_secs(1));
                            Ok(())
                        })?;
                workers.push(worker);
            }
            let mut unexpected = false;
            let signal = async {
                #[cfg(unix)]
                tokio::select! {
                    _ = interrupt.recv() => {},
                    _ = terminate.recv() => {},
                    _ = quit.recv() => {},
                }
                #[cfg(not(unix))]
                tokio::signal::ctrl_c().await?;
                Ok::<_, std::io::Error>(())
            };
            tokio::pin!(signal);
            loop {
                tokio::select! {
                    result = &mut signal => { result?; break; }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if stopped.load(std::sync::atomic::Ordering::Acquire) != 0
                            || workers.iter().any(|worker| worker.is_finished()) {
                            unexpected = true;
                            break;
                        }
                    }
                }
            }
            shared.telemetry.draining.set(1);
            let grace =
                tokio::time::Instant::now() + Duration::from_secs(limits.shutdown_grace_seconds);
            while shared.requests.available_permits() < limits.max_inflight
                && tokio::time::Instant::now() < grace
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let _ = shutdown.send(true);
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(limits.shutdown_timeout_seconds);
            while stopped.load(std::sync::atomic::Ordering::Acquire) < workers.len()
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let _ = finish.send(true);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while workers.iter().any(|worker| !worker.is_finished())
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            for worker in workers {
                if worker.is_finished() {
                    worker
                        .join()
                        .map_err(|_| anyhow::anyhow!("service thread panicked"))??;
                } else {
                    log::warn!("service exceeded shutdown deadline");
                }
            }
            anyhow::ensure!(!unexpected, "a runtime service stopped unexpectedly");
            Ok(())
        })
        .context("native Hyper runtime")
}
