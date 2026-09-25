use super::{block, duration, leaf, one, syntax::Directive};
use crate::{
    backend::{Backend, Balance, HealthCheck, Origin},
    model::Endpoint,
    traffic::Key,
};
use anyhow::{Context, Result, bail, ensure};
use std::{net::ToSocketAddrs, time::Duration};

pub(super) fn load(n: &Directive) -> Result<Backend> {
    one(n)?;
    let mut origins = Vec::new();
    let mut options = crate::backend::Options::default();
    for node in block(n)? {
        let result = (|| -> Result<()> {
            leaf(node)?;
            match node.name.as_str() {
                "server" => {
                    ensure!(
                        (1..=2).contains(&node.args.len()),
                        "server expects address [weight=N]"
                    );
                    let weight = node
                        .args
                        .get(1)
                        .map(|w| -> Result<u32> {
                            Ok(w.strip_prefix("weight=")
                                .context("only weight=N is supported")?
                                .parse()?)
                        })
                        .transpose()?
                        .unwrap_or(1);
                    ensure!((1..=65535).contains(&weight), "invalid weight");
                    let parsed = url::Url::parse(&format!("http://{}", node.args[0]))?;
                    ensure!(
                        parsed.path() == "/"
                            && parsed.query().is_none()
                            && parsed.fragment().is_none()
                            && parsed.username().is_empty()
                            && parsed.password().is_none(),
                        "server expects a host and optional port"
                    );
                    let host = parsed
                        .host_str()
                        .context("missing upstream host")?
                        .trim_matches(['[', ']'])
                        .to_owned();
                    origins.push(Origin {
                        host,
                        port: parsed.port_or_known_default().unwrap(),
                        weight,
                    });
                }
                "least_conn" | "ip_hash" => {
                    ensure!(node.args.is_empty(), "{} takes no arguments", node.name);
                    options.balance = if node.name == "least_conn" {
                        Balance::LeastConnections
                    } else {
                        Balance::Hash(Key::Ip)
                    };
                }
                "rgnix_balance" => {
                    options.balance = match node
                        .args
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .as_slice()
                    {
                        ["round_robin"] => Balance::RoundRobin,
                        ["least_conn"] => Balance::LeastConnections,
                        ["hash", key] => Balance::Hash(Key::parse(key)?),
                        ["sticky", cookie] => {
                            http::header::HeaderName::from_bytes(cookie.as_bytes())?;
                            ensure!(cookie.len() <= 64, "affinity cookie name exceeds 64 bytes");
                            Balance::Sticky((*cookie).into())
                        }
                        _ => bail!(
                            "rgnix_balance expects round_robin, least_conn, hash key or sticky cookie"
                        ),
                    };
                }
                "rgnix_max_inflight" => {
                    one(node)?;
                    options.max_inflight = node.args[0].parse()?;
                }
                "rgnix_health_check" => {
                    ensure!(
                        (1..=4).contains(&node.args.len()),
                        "health check expects path [interval=10s] [timeout=1s] [status=200]"
                    );
                    let mut check = HealthCheck {
                        path: node.args[0].clone(),
                        interval: Duration::from_secs(10),
                        timeout: Duration::from_secs(1),
                        status: 200,
                    };
                    for option in &node.args[1..] {
                        if let Some(v) = option.strip_prefix("interval=") {
                            check.interval = duration(v)?;
                        } else if let Some(v) = option.strip_prefix("timeout=") {
                            check.timeout = duration(v)?;
                        } else if let Some(v) = option.strip_prefix("status=") {
                            check.status = v.parse()?;
                        } else {
                            bail!("unknown health option {option}");
                        }
                    }
                    options.health = Some(check);
                }
                _ => bail!("unsupported upstream directive {}", node.name),
            }
            Ok(())
        })();
        result.with_context(|| node.source.clone())?;
    }
    options.validate()?;
    ensure!(
        !origins.is_empty() && origins.len() <= 1024,
        "upstream requires 1..1024 servers"
    );
    let mut endpoints = Vec::new();
    for origin in &origins {
        for address in (origin.host.as_str(), origin.port).to_socket_addrs()? {
            endpoints.push(Endpoint {
                address,
                weight: origin.weight,
            });
        }
    }
    ensure!(!endpoints.is_empty(), "upstream has no addresses");
    let name = n.args[0].clone();
    let mut backend = Backend::new(endpoints, false, name.clone(), name);
    backend.options = options;
    backend.origins = origins;
    Ok(backend)
}
