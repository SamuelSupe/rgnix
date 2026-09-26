use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rgnix::{
    config, ingress,
    model::{Listener, RuntimeSnapshot},
    runtime::{self, Source},
    script::{self, Compiler},
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(
    name = "rgnix",
    version,
    about = "Programmable HTTP server and Kubernetes Ingress controller"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Wait for all discovered controller replicas to accept the expected configuration digest.
    Wait(rgnix::fleet::WaitOptions),
    /// Compile or run an administrator-owned RGL packet filter in Linux XDP.
    Xdp {
        #[command(subcommand)]
        command: rgnix::xdp::Command,
    },
    /// Assess or convert existing configurations without changing a running deployment.
    Migrate {
        #[command(subcommand)]
        command: rgnix::migration::Command,
    },
    /// Run a pre-provisioned Gateway API data plane bound to namespace/name.
    Gateway {
        #[arg(long)]
        gateway: String,
        #[arg(long)]
        publish_service: String,
        #[arg(long = "watch-namespace")]
        watch_namespaces: Vec<String>,
        #[arg(long, default_value = "0.0.0.0:8080")]
        http_listen: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:8443")]
        https_listen: SocketAddr,
        #[arg(long, default_value_t = 80)]
        http_port: u16,
        #[arg(long, default_value_t = 443)]
        https_port: u16,
        #[arg(long, default_value = "0.0.0.0:9090")]
        admin: SocketAddr,
        #[arg(long, env = "POD_NAME")]
        identity: Option<String>,
        #[command(flatten)]
        forwarding: rgnix::identity::Forwarding,
        #[command(flatten)]
        limits: runtime::Limits,
        #[command(flatten)]
        otlp: rgnix::otlp::Options,
        #[command(flatten)]
        diagnostics: rgnix::diagnostics::Options,
    },
    Serve {
        #[arg(short = 'c', long)]
        config: PathBuf,
        #[arg(long, default_value = "127.0.0.1:9090")]
        admin: SocketAddr,
        #[command(flatten)]
        limits: runtime::Limits,
        #[command(flatten)]
        otlp: rgnix::otlp::Options,
        #[command(flatten)]
        diagnostics: rgnix::diagnostics::Options,
    },
    Check {
        #[arg(short = 'c', long)]
        config: PathBuf,
    },
    Diff {
        #[arg(short = 'c', long)]
        config: PathBuf,
        #[arg(long)]
        against: PathBuf,
    },
    Dump {
        #[arg(short = 'c', long)]
        config: PathBuf,
    },
    Explain {
        #[arg(short = 'c', long)]
        config: PathBuf,
        #[arg(long)]
        listener: Option<SocketAddr>,
        #[arg(long, default_value = "")]
        host: String,
        #[arg(long, default_value = "/")]
        path: String,
    },
    Simulate {
        #[arg(short = 'c', long)]
        config: PathBuf,
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        live_auth: bool,
    },
    Compile {
        source: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
    },
    Ingress {
        #[arg(long, default_value = "rgnix")]
        ingress_class: String,
        /// Restrict application watches to these namespaces; repeat for multiple namespaces.
        #[arg(long = "watch-namespace")]
        watch_namespaces: Vec<String>,
        #[arg(long)]
        publish_service: String,
        #[arg(long, default_value = "0.0.0.0:8080")]
        http_listen: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:8443")]
        https_listen: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:9090")]
        admin: SocketAddr,
        #[command(flatten)]
        forwarding: rgnix::identity::Forwarding,
        #[arg(long, env = "POD_NAME")]
        identity: Option<String>,
        #[command(flatten)]
        limits: runtime::Limits,
        #[command(flatten)]
        otlp: rgnix::otlp::Options,
        #[command(flatten)]
        diagnostics: rgnix::diagnostics::Options,
    },
}
fn main() -> Result<()> {
    rgnix::logging::init()?;
    let cli = Cli::parse();
    match cli.command {
        Command::Wait(options) => options.run(),
        Command::Xdp { command } => rgnix::xdp::run(command),
        Command::Migrate { command } => rgnix::migration::run(command),
        Command::Gateway {
            gateway,
            publish_service,
            watch_namespaces,
            http_listen,
            https_listen,
            http_port,
            https_port,
            admin,
            identity,
            forwarding,
            limits,
            otlp,
            diagnostics,
        } => {
            let (namespace, name) = gateway
                .split_once('/')
                .context("gateway must be namespace/name")?;
            let (publish_namespace, publish_service) = publish_service
                .split_once('/')
                .context("publish-service must be namespace/name")?;
            anyhow::ensure!(
                [namespace, name, publish_namespace, publish_service]
                    .into_iter()
                    .chain(watch_namespaces.iter().map(String::as_str))
                    .all(rgnix::tenancy::namespace_name),
                "invalid Kubernetes name or namespace"
            );
            anyhow::ensure!(
                http_port > 0
                    && https_port > 0
                    && http_port != https_port
                    && http_listen != https_listen,
                "HTTP and HTTPS need distinct nonzero ports and addresses"
            );
            let options = rgnix::gateway::Options {
                namespace: namespace.into(),
                name: name.into(),
                namespaces: watch_namespaces,
                publish_namespace: publish_namespace.into(),
                publish_service: publish_service.into(),
                identity: identity.unwrap_or_else(|| format!("rgnix-{}", std::process::id())),
                http_port,
                https_port,
                identity_policy: forwarding.policy()?,
            };
            let listeners = [(http_listen, false), (https_listen, true)]
                .into_iter()
                .map(|(address, tls)| Listener {
                    address,
                    tls,
                    http2: true,
                    proxy_protocol: forwarding.proxy_protocol,
                    proxy_trusted: forwarding.trusted_proxy.clone(),
                })
                .collect();
            runtime::serve(
                RuntimeSnapshot::empty(listeners),
                Arc::new(Compiler::for_runtime(limits.max_plugin_instances)?),
                Source::Gateway(options),
                admin,
                limits,
                otlp,
                diagnostics,
            )
        }
        Command::Diff { config, against } => {
            let compiler = Compiler::new()?;
            let before = rgnix::config::load(&against, &compiler, 1)?;
            let after = rgnix::config::load(&config, &compiler, 1)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&rgnix::diagnostics::preflight::diff(
                    &before, &after
                ))?
            );
            Ok(())
        }
        Command::Dump { config: path } => {
            let snapshot = config::load(&path, &Compiler::new()?, 1)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&rgnix::diagnostics::describe(&snapshot))?
            );
            Ok(())
        }
        Command::Explain {
            config: path,
            listener,
            host,
            path: uri,
        } => {
            let snapshot = config::load(&path, &Compiler::new()?, 1)?;
            let listener = listener
                .or_else(|| snapshot.listeners.first().map(|l| l.address))
                .context("missing listener")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&rgnix::diagnostics::explain(
                    &snapshot, listener, &host, &uri
                )?)?
            );
            Ok(())
        }
        Command::Simulate {
            config: path,
            request,
            live_auth,
        } => {
            let snapshot = config::load(&path, &Compiler::new()?, 1)?;
            let bytes = std::fs::read(request)?;
            anyhow::ensure!(
                bytes.len() <= 1024 * 1024,
                "simulation fixture exceeds 1 MiB"
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&tokio::runtime::Runtime::new()?.block_on(
                    rgnix::diagnostics::simulate(
                        &snapshot,
                        serde_json::from_slice(&bytes)?,
                        live_auth,
                    )
                )?)?
            );
            Ok(())
        }
        Command::Compile { source, output } => {
            let text = script::read_source(&source)?;
            let bytes = script::compile(&text).with_context(|| source.display().to_string())?;
            Compiler::new()?.from_bytes(&bytes, true)?;
            std::fs::write(&output, &bytes)?;
            println!(
                "compiled {} -> {} ({} bytes)",
                source.display(),
                output.display(),
                bytes.len()
            );
            Ok(())
        }
        Command::Check { config: path } => {
            let compiler = Compiler::new()?;
            let snapshot = config::load(&path, &compiler, 1)?;
            println!(
                "configuration valid: {} listeners, {} virtual hosts, {} backends",
                snapshot.listeners.len(),
                snapshot.hosts.len(),
                snapshot.backends.len()
            );
            Ok(())
        }
        Command::Serve {
            config: path,
            admin,
            limits,
            otlp,
            diagnostics,
        } => {
            let compiler = Arc::new(Compiler::for_runtime(limits.max_plugin_instances)?);
            let path = path.canonicalize().or_else(|error| {
                if diagnostics.history_dir.is_some() {
                    Ok(std::env::current_dir()?
                        .join(&path)
                        .parent()
                        .unwrap()
                        .canonicalize()?
                        .join(path.file_name().unwrap()))
                } else {
                    Err(error)
                }
            })?;
            let snapshot =
                rgnix::history::initial(&path, diagnostics.history_dir.as_deref(), &compiler)?;
            runtime::serve(
                snapshot,
                compiler,
                Source::File(path),
                admin,
                limits,
                otlp,
                diagnostics,
            )
        }
        Command::Ingress {
            ingress_class,
            watch_namespaces,
            publish_service,
            http_listen,
            https_listen,
            admin,
            identity,
            forwarding,
            limits,
            otlp,
            diagnostics,
        } => {
            let (namespace, service) = publish_service
                .split_once('/')
                .context("publish-service must be namespace/name")?;
            anyhow::ensure!(
                !namespace.is_empty() && !service.is_empty(),
                "publish-service must be namespace/name"
            );
            let identity = identity.unwrap_or_else(|| format!("rgnix-{}", std::process::id()));
            let identity_policy = forwarding.policy()?;
            let listeners = vec![
                Listener {
                    address: http_listen,
                    tls: false,
                    http2: false,
                    proxy_protocol: forwarding.proxy_protocol,
                    proxy_trusted: forwarding.trusted_proxy.clone(),
                },
                Listener {
                    address: https_listen,
                    tls: true,
                    http2: true,
                    proxy_protocol: forwarding.proxy_protocol,
                    proxy_trusted: forwarding.trusted_proxy.clone(),
                },
            ];
            anyhow::ensure!(
                watch_namespaces
                    .iter()
                    .all(|n| rgnix::tenancy::namespace_name(n)),
                "invalid watched namespace"
            );
            let options = ingress::Options {
                namespaces: watch_namespaces,
                class: ingress_class,
                publish_namespace: namespace.into(),
                publish_service: service.into(),
                identity,
                identity_policy,
            };
            runtime::serve(
                RuntimeSnapshot::empty(listeners),
                Arc::new(Compiler::for_runtime(limits.max_plugin_instances)?),
                Source::Ingress(options),
                admin,
                limits,
                otlp,
                diagnostics,
            )
        }
    }
}
