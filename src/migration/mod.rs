mod ingress;
mod nginx;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub enum Command {
    /// Inventory NGINX directives, report differences, and write a validated candidate.
    Nginx {
        #[arg(short = 'c', long)]
        config: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Explicitly discard NGINX process-model directives; review the reported differences.
        #[arg(long)]
        accept_process_differences: bool,
    },
    /// Convert standard Ingress YAML/JSON to Gateway API. Service manifests resolve named ports.
    Ingress {
        #[arg(short = 'f', long)]
        file: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long, default_value = "rgnix")]
        gateway_class: String,
        #[arg(long, default_value = "rgnix")]
        gateway_name: String,
        #[arg(long, default_value = "default")]
        namespace: String,
    },
    /// Compare observable offline simulation results for a JSON array of request fixtures.
    Compare {
        #[arg(long)]
        before: PathBuf,
        #[arg(long)]
        after: PathBuf,
        #[arg(long)]
        requests: PathBuf,
    },
}

pub fn run(command: Command) -> Result<()> {
    let report = match command {
        Command::Nginx {
            config,
            output,
            accept_process_differences,
        } => nginx::assess(&config, output.as_deref(), accept_process_differences)?,
        Command::Ingress {
            file,
            output,
            gateway_class,
            gateway_name,
            namespace,
        } => ingress::convert(
            &file,
            output.as_deref(),
            &gateway_class,
            &gateway_name,
            &namespace,
        )?,
        Command::Compare {
            before,
            after,
            requests,
        } => {
            let compiler = crate::script::Compiler::new()?;
            let before = crate::config::load(&before, &compiler, 1)?;
            let after = crate::config::load(&after, &compiler, 1)?;
            let fixtures: Vec<Value> = serde_json::from_slice(&crate::controls::read_bounded(
                &requests,
                4 * 1024 * 1024,
            )?)?;
            ensure!(fixtures.len() <= 1000, "at most 1000 request fixtures");
            let runtime = tokio::runtime::Runtime::new()?;
            let mut cases = vec![];
            for (index, fixture) in fixtures.into_iter().enumerate() {
                let a = runtime
                    .block_on(crate::diagnostics::simulate(
                        &before,
                        serde_json::from_value(fixture.clone())?,
                        false,
                    ))
                    .with_context(|| format!("before fixture {index}"))?;
                let b = runtime
                    .block_on(crate::diagnostics::simulate(
                        &after,
                        serde_json::from_value(fixture)?,
                        false,
                    ))
                    .with_context(|| format!("after fixture {index}"))?;
                // Route IDs and versions identify configuration objects, not request behavior.
                let normalize = |mut result: Value| {
                    if let Some(o) = result.as_object_mut() {
                        o.remove("version");
                        o.remove("route");
                    }
                    result
                };
                let a = normalize(a);
                let b = normalize(b);
                cases.push(json!({"index":index,"equal":a==b,"before":a,"after":b}));
            }
            json!({"kind":"request-comparison","compatible":cases.iter().all(|c| c["equal"]==true),"cases":cases,"scope":"offline rgnix simulations; external authentication uses fixtures; no upstream traffic or NGINX runtime is exercised"})
        }
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(
        report["compatible"] == true,
        "migration requires manual review; see JSON report"
    );
    Ok(())
}

fn write_new(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| {
            format!(
                "create {} (existing files are never overwritten)",
                path.display()
            )
        })?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
