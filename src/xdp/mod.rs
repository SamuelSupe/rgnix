mod artifact;
mod compiler;
mod config;
#[cfg(target_os = "linux")]
mod diagnostics;
#[cfg(target_os = "linux")]
mod events;
mod history;
#[cfg(target_os = "linux")]
mod kernel;
#[cfg(target_os = "linux")]
mod lifecycle;
#[cfg(target_os = "linux")]
mod management;
#[cfg(target_os = "linux")]
mod maps;
#[cfg(target_os = "linux")]
mod service;

use anyhow::{Context, Result, ensure};
use clap::{Subcommand, ValueEnum};
use std::{
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Mode {
    Native,
    Generic,
}

#[derive(Subcommand)]
pub enum Command {
    /// Compile the on_xdp() RGL subset to a portable little-endian eBPF ELF object.
    Compile {
        source: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Build an immutable libxdp component instead of a managed-agent artifact.
        #[arg(long)]
        dispatcher: bool,
        #[arg(long, requires = "dispatcher")]
        config: Option<PathBuf>,
        #[arg(long, default_value = "clang")]
        clang: PathBuf,
    },
    /// Validate a policy without attaching; --kernel also runs the kernel verifier.
    Check {
        source: PathBuf,
        #[arg(long)]
        kernel: bool,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, default_value = "clang")]
        clang: PathBuf,
    },
    /// Own one interface attachment until exit; SIGHUP atomically replaces this policy.
    Run {
        source: PathBuf,
        #[arg(long)]
        interface: String,
        #[arg(long, value_enum, default_value = "native")]
        mode: Mode,
        #[arg(long, default_value = "127.0.0.1:9191")]
        admin: SocketAddr,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(0..=3600))]
        watch_interval: u64,
        #[arg(long)]
        pin_dir: Option<PathBuf>,
        #[arg(long)]
        persist: bool,
        #[arg(long)]
        history_dir: Option<PathBuf>,
        #[command(flatten)]
        otlp: crate::otlp::Options,
        #[arg(long, default_value = "clang")]
        clang: PathBuf,
    },
    /// Execute a raw Ethernet frame through the real kernel program without attaching.
    Test {
        source: PathBuf,
        #[arg(long)]
        packet: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=100_000))]
        repeat: u32,
        #[arg(long, default_value = "clang")]
        clang: PathBuf,
    },
    /// Read kernel, interface, existing XDP/CNI and bpffs capabilities without attaching.
    Doctor {
        #[arg(long)]
        interface: String,
    },
    /// Read the agent's actual and desired revisions, errors, and rule counters.
    Status {
        #[arg(long, default_value = "127.0.0.1:9191")]
        admin: SocketAddr,
        #[arg(long, conflicts_with = "health")]
        ready: bool,
        #[arg(long)]
        health: bool,
    },
    /// Replay a classic Ethernet PCAP through the kernel and explain matched rules.
    Replay {
        source: PathBuf,
        #[arg(long)]
        pcap: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, default_value = "clang")]
        clang: PathBuf,
    },
    /// Remove only the persistent link and state owned by this interface.
    Detach {
        #[arg(long)]
        interface: String,
        #[arg(long, value_enum, default_value = "native")]
        mode: Mode,
        #[arg(long)]
        pin_dir: Option<PathBuf>,
    },
    /// Atomically point a watched configuration at an immutable saved policy revision.
    Rollback {
        #[arg(long)]
        history_dir: PathBuf,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        config: PathBuf,
    },
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Compile {
            source,
            output,
            dispatcher,
            config,
            clang,
        } => {
            let settings = config::Config::read(config.as_deref())?;
            let object = compiler::compile_mode(
                &crate::script::read_source(&source)
                    .with_context(|| source.display().to_string())?,
                &clang,
                dispatcher.then_some(&settings),
            )
            .with_context(|| source.display().to_string())?;
            let parent = output
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            std::io::Write::write_all(&mut file, &object)?;
            file.as_file().sync_all()?;
            file.persist(&output)?;
            println!(
                "compiled {} -> {} ({} bytes)",
                source.display(),
                output.display(),
                object.len()
            );
            Ok(())
        }
        Command::Check {
            source,
            kernel,
            config,
            clang,
        } => {
            ensure!(
                kernel || source.extension().is_none_or(|s| s != "o"),
                "precompiled objects require --kernel for validation"
            );
            let candidate =
                artifact::Inputs::read(&source, config.as_deref())?.prepare(&clang, None)?;
            let object = &candidate.object;
            if kernel {
                verify(candidate.clone())?;
            }
            println!(
                "XDP policy valid: {} ({} bytes, kernel verification: {kernel})",
                source.display(),
                object.len()
            );
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Command::Run {
            source,
            interface,
            mode,
            admin,
            config,
            watch_interval,
            pin_dir,
            persist,
            history_dir,
            otlp,
            clang,
        } => service::run(service::Options {
            source,
            interface,
            mode,
            admin,
            config,
            watch_interval,
            pin_dir,
            persist,
            history_dir,
            otlp,
            clang,
        }),
        #[cfg(target_os = "linux")]
        Command::Test {
            source,
            packet,
            config,
            repeat,
            clang,
        } => {
            let loaded = kernel::Loaded::configured(
                artifact::Inputs::read(&source, config.as_deref())?.prepare(&clang, None)?,
                None,
            )?;
            let mut frame = Vec::new();
            std::fs::File::open(packet)?
                .take(65537)
                .read_to_end(&mut frame)?;
            ensure!(frame.len() <= 65536, "packet exceeds 64 KiB");
            println!(
                "{}",
                serde_json::to_string_pretty(&loaded.test(&frame, repeat)?)?
            );
            Ok(())
        }
        Command::Rollback {
            history_dir,
            revision,
            config,
        } => history::rollback(&history_dir, &revision, &config),
        #[cfg(target_os = "linux")]
        Command::Doctor { interface } => diagnostics::doctor(&interface),
        #[cfg(target_os = "linux")]
        Command::Status {
            admin,
            ready,
            health,
        } => diagnostics::status(admin, ready, health),
        #[cfg(target_os = "linux")]
        Command::Replay {
            source,
            pcap,
            config,
            clang,
        } => diagnostics::replay(&source, &pcap, config.as_deref(), &clang),
        #[cfg(target_os = "linux")]
        Command::Detach {
            interface,
            mode,
            pin_dir,
        } => lifecycle::Pins::open(&interface, mode, pin_dir, true)?.detach(),
        #[cfg(not(target_os = "linux"))]
        _ => anyhow::bail!(
            "XDP execution requires Linux with BPF links and atomics (kernel 5.12+); compilation can run wherever Clang supports bpfel"
        ),
    }
}

fn read_object(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "XDP object exceeds 4 MiB");
    ensure!(bytes.starts_with(b"\x7fELF"), "expected an eBPF ELF object");
    Ok(bytes)
}
fn verify(candidate: artifact::Candidate) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        kernel::Loaded::configured(candidate, None)?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = candidate;
        anyhow::bail!("kernel verification requires Linux")
    }
}
