//! Socket-test fixture: attach Gateway deadlines to compiled file routes without
//! replacing the production data plane or requiring a Kubernetes control plane.
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
struct DeadlineFixture {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    admin: SocketAddr,
    #[arg(long)]
    request_ms: Option<u64>,
    #[arg(long)]
    backend_ms: Option<u64>,
    #[command(flatten)]
    limits: rgnix::runtime::Limits,
    #[command(flatten)]
    otlp: rgnix::otlp::Options,
    #[command(flatten)]
    diagnostics: rgnix::diagnostics::Options,
}

fn main() -> anyhow::Result<()> {
    rgnix::logging::init()?;
    let mut options = DeadlineFixture::parse();
    options.limits.engine.engine = Some(rgnix::runtime::Engine::Hyper);
    let compiler = Arc::new(rgnix::script::Compiler::for_runtime(
        options.limits.max_plugin_instances,
    )?);
    let mut snapshot = rgnix::config::load(&options.config, &compiler, 1)?;
    for host in &mut snapshot.hosts {
        for route in &mut host.routes {
            let route = Arc::make_mut(route);
            let rgnix::model::Action::Proxy { backend, .. } = &route.action else {
                continue;
            };
            let mut policy = rgnix::gateway::Policy {
                backends: vec![(Some(backend.clone()), 1)],
                listener_port: host.listener.port(),
                ..Default::default()
            };
            policy.timeouts.request = options
                .request_ms
                .map(Duration::from_millis)
                .filter(|d| !d.is_zero());
            policy.timeouts.backend_request = options
                .backend_ms
                .map(Duration::from_millis)
                .filter(|d| !d.is_zero());
            route.settings.gateway = Some(policy);
        }
    }
    rgnix::runtime::serve(
        snapshot,
        compiler,
        rgnix::runtime::Source::File(options.config),
        options.admin,
        options.limits,
        options.otlp,
        options.diagnostics,
    )
}
