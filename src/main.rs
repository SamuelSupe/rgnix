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
    Serve {
        #[arg(short = 'c', long)]
        config: PathBuf,
        #[arg(long, default_value = "127.0.0.1:9090")]
        admin: SocketAddr,
        #[command(flatten)]
        limits: runtime::Limits,
    },
    Check {
        #[arg(short = 'c', long)]
        config: PathBuf,
    },
    Compile {
        source: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
    },
    Ingress {
        #[arg(long, default_value = "rgnix")]
        ingress_class: String,
        #[arg(long)]
        publish_service: String,
        #[arg(long, default_value = "0.0.0.0:8080")]
        http_listen: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:8443")]
        https_listen: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:9090")]
        admin: SocketAddr,
        #[arg(long, env = "POD_NAME")]
        identity: Option<String>,
        #[command(flatten)]
        limits: runtime::Limits,
    },
}
fn main() -> Result<()> {
    rgnix::logging::init()?;
    let cli = Cli::parse();
    match cli.command {
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
        } => {
            let compiler = Arc::new(Compiler::new()?);
            let path = path.canonicalize()?;
            let snapshot = config::load(&path, &compiler, 1)?;
            runtime::serve(snapshot, compiler, Source::File(path), admin, limits)
        }
        Command::Ingress {
            ingress_class,
            publish_service,
            http_listen,
            https_listen,
            admin,
            identity,
            limits,
        } => {
            let (namespace, service) = publish_service
                .split_once('/')
                .context("publish-service must be namespace/name")?;
            anyhow::ensure!(
                !namespace.is_empty() && !service.is_empty(),
                "publish-service must be namespace/name"
            );
            let identity = identity.unwrap_or_else(|| format!("rgnix-{}", std::process::id()));
            let listeners = vec![
                Listener {
                    address: http_listen,
                    tls: false,
                    http2: false,
                },
                Listener {
                    address: https_listen,
                    tls: true,
                    http2: true,
                },
            ];
            let options = ingress::Options {
                class: ingress_class,
                publish_namespace: namespace.into(),
                publish_service: service.into(),
                identity,
            };
            runtime::serve(
                RuntimeSnapshot::empty(listeners),
                Arc::new(Compiler::new()?),
                Source::Ingress(options),
                admin,
                limits,
            )
        }
    }
}
