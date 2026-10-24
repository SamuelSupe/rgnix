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
fn metric_thread_hooks(builder: &mut tokio::runtime::Builder, range: std::ops::Range<usize>) {
    // Blocking threads retire and restart; returning slots prevents their
    // replacements from consuming another listener's assigned range.
    let slots = Arc::new(std::sync::Mutex::new(range.rev().collect::<Vec<_>>()));
    let starting = slots.clone();
    builder.on_thread_start(move || {
        let slot = starting.lock().unwrap().pop().unwrap_or(0);
        crate::telemetry::sharded::register_worker(slot);
    });
    builder.on_thread_stop(move || {
        let slot = crate::telemetry::sharded::unregister_worker();
        if slot != 0 {
            slots.lock().unwrap().push(slot);
        }
    });
}

#[cfg(feature = "hyper-experimental")]
fn metric_service_slots(name: &str, threads: usize) -> usize {
    // background_service() prefixes the name exposed by ServiceWithDependents.
    let name = name.strip_prefix("BG ").unwrap_or(name);
    if name.starts_with("hyper-") {
        1
    } else if name.starts_with("http3-") {
        1 + (if threads > 1 { threads } else { 0 }) + (16 / threads).clamp(1, 2)
    } else {
        0
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
            let _wakeup_probe = shared.telemetry.data_plane.enable(
                Duration::from_secs(limits.hyper_worker_stall_timeout_seconds),
                limits.hyper_wakeup_probe,
            )?;
            let heartbeats: Vec<_> = services
                .iter()
                .map(|s| shared.telemetry.data_plane.register(s.name().to_owned()))
                .collect();
            crate::telemetry::sharded::register_worker(0);
            let mut next_metric_slot = 0;
            for (mut service, heartbeat) in services.into_iter().zip(heartbeats) {
                let name = service.name().to_owned();
                let threads = service.threads().unwrap_or(limits.threads);
                let metric_slots = metric_service_slots(&name, threads);
                let metric_slot = if metric_slots > 0 {
                    next_metric_slot
                } else {
                    0
                };
                next_metric_slot += metric_slots;
                let runtime_slots = metric_slots.saturating_sub(1);
                let watch = watch.clone();
                let mut finished = finished.clone();
                let stopped = stopped.clone();
                let worker =
                    std::thread::Builder::new()
                        .name(name)
                        .spawn(move || -> Result<()> {
                            crate::telemetry::sharded::register_worker(metric_slot);
                            let mut builder = if threads == 1 {
                                let mut builder = tokio::runtime::Builder::new_current_thread();
                                // Bound I/O deferral without polling the reactor after
                                // every few proxy exchanges under sustained load.
                                builder.event_interval(32);
                                builder
                            } else {
                                let mut builder = tokio::runtime::Builder::new_multi_thread();
                                builder.worker_threads(threads);
                                builder
                            };
                            metric_thread_hooks(
                                &mut builder,
                                metric_slot + 1..metric_slot + 1 + runtime_slots,
                            );
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

#[cfg(all(test, feature = "hyper-experimental"))]
mod tests {
    use super::*;

    #[test]
    fn background_listeners_reserve_distinct_metric_slots() {
        use pingora::services::background::{BackgroundService, background_service};
        struct Idle;
        #[async_trait::async_trait]
        impl BackgroundService for Idle {}
        let services = [
            background_service("control", Idle),
            background_service("hyper-127.0.0.1:8080-0", Idle),
            background_service("hyper-127.0.0.1:8080-1", Idle),
            background_service("hyper-127.0.0.1:8081-0", Idle),
            background_service("hyper-127.0.0.1:8081-1", Idle),
            background_service("http3-127.0.0.1:8443", Idle),
        ];
        let mut next = 0;
        let ranges: Vec<_> = services
            .iter()
            .map(|service| {
                let count = metric_service_slots(service.name(), 2);
                let range = next..next + count;
                next += count;
                range
            })
            .collect();
        assert_eq!(ranges, [0..0, 0..1, 1..2, 2..3, 3..4, 4..9]);
        assert_eq!(metric_service_slots("BG http3-127.0.0.1:8443", 1), 3);
    }

    #[test]
    fn retired_blocking_threads_reuse_their_assigned_metric_slot() {
        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder
            .max_blocking_threads(1)
            .thread_keep_alive(Duration::from_millis(10));
        metric_thread_hooks(&mut builder, 1..2);
        let runtime = builder.build().unwrap();
        let mut threads = std::collections::HashSet::new();
        for _ in 0..4 {
            let (thread, slot) = runtime.block_on(async {
                tokio::task::spawn_blocking(|| {
                    let slot = crate::telemetry::sharded::unregister_worker();
                    crate::telemetry::sharded::register_worker(slot);
                    (std::thread::current().id(), slot)
                })
                .await
                .unwrap()
            });
            assert_eq!(slot, 1);
            threads.insert(thread);
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(threads.len() > 1, "the test must exercise a retired worker");
    }
}
