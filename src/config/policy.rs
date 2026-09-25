use super::{Scope, one, syntax::Directive};
use crate::{
    identity::{self, AccessRule},
    traffic::{Concurrency, Key, Rate},
};
use anyhow::{Result, bail, ensure};

pub(super) fn apply(s: &mut Scope, n: &Directive) -> Result<bool> {
    match n.name.as_str() {
        "rgnix_log_query" | "rgnix_log_client" | "rgnix_log_referer" => {
            one(n)?;
            let value = switch(&n.args[0])?;
            match n.name.as_str() {
                "rgnix_log_query" => s.settings.log_policy.query = value,
                "rgnix_log_client" => s.settings.log_policy.client = value,
                _ => s.settings.log_policy.referer = value,
            }
        }
        "rgnix_log_redact" | "rgnix_log_fields" => {
            if n.name == "rgnix_log_redact" {
                s.settings.log_policy.redact = n.args.clone();
            } else {
                s.settings.log_policy.fields = n.args.clone();
            }
            s.settings.log_policy.validate_fields()?;
        }
        "gzip" | "brotli" => {
            one(n)?;
            let level = if switch(&n.args[0])? {
                if n.name == "gzip" { 1 } else { 4 }
            } else {
                0
            };
            if n.name == "gzip" {
                s.settings.compression.gzip = level;
            } else {
                s.settings.compression.brotli = level;
            }
        }
        "gzip_comp_level" | "brotli_comp_level" => {
            one(n)?;
            let level = n.args[0].parse()?;
            ensure!((1..=9).contains(&level), "compression level must be 1..9");
            if n.name == "gzip_comp_level" {
                s.settings.compression.gzip = level;
            } else {
                s.settings.compression.brotli = level;
            }
        }
        "gzip_min_length" => {
            one(n)?;
            s.settings.compression.min_length = super::size(&n.args[0])?;
        }
        "gzip_types" => {
            ensure!(!n.args.is_empty(), "gzip_types expects MIME types");
            s.settings.compression.types = n.args.clone();
            if !s
                .settings
                .compression
                .types
                .iter()
                .any(|t| t == "text/html")
            {
                s.settings.compression.types.push("text/html".into());
            }
        }
        "set_real_ip_from" => {
            one(n)?;
            s.settings
                .identity
                .trusted
                .push(identity::network(&n.args[0])?);
        }
        "real_ip_header" => {
            one(n)?;
            http::header::HeaderName::from_bytes(n.args[0].as_bytes())?;
            s.settings.identity.header = n.args[0].to_ascii_lowercase();
        }
        "real_ip_recursive" => {
            one(n)?;
            s.settings.identity.recursive = switch(&n.args[0])?;
        }
        "allow" | "deny" => {
            one(n)?;
            s.settings.identity.access.push(AccessRule {
                network: if n.args[0] == "all" {
                    None
                } else {
                    Some(identity::network(&n.args[0])?)
                },
                allow: n.name == "allow",
            });
        }
        "rgnix_limit_rate" => {
            s.settings.traffic.rate = if n.args == ["off"] {
                None
            } else {
                ensure!(
                    (1..=3).contains(&n.args.len()),
                    "rgnix_limit_rate expects requests-per-second [burst=N] [key=ip|route|header:NAME|cookie:NAME|jwt:CLAIM]"
                );
                let per_second = n.args[0].parse()?;
                let mut rate = Rate {
                    per_second,
                    burst: per_second,
                    key: Key::Ip,
                };
                for arg in &n.args[1..] {
                    if let Some(v) = arg.strip_prefix("burst=") {
                        rate.burst = v.parse()?;
                    } else if let Some(v) = arg.strip_prefix("key=") {
                        rate.key = Key::parse(v)?;
                    } else {
                        bail!("unsupported rate option {arg}");
                    }
                }
                Some(rate)
            };
        }
        "rgnix_limit_conn" => {
            s.settings.traffic.concurrency = if n.args == ["off"] {
                None
            } else {
                ensure!(
                    (1..=2).contains(&n.args.len()),
                    "rgnix_limit_conn expects maximum [key=...]"
                );
                let key = n
                    .args
                    .get(1)
                    .map(|v| {
                        v.strip_prefix("key=")
                            .ok_or_else(|| anyhow::anyhow!("expected key=..."))
                            .and_then(Key::parse)
                    })
                    .transpose()?
                    .unwrap_or(Key::Ip);
                Some(Concurrency {
                    limit: n.args[0].parse()?,
                    key,
                })
            };
        }
        _ => return Ok(false),
    }
    s.settings.traffic.validate()?;
    s.settings.identity.validate()?;
    Ok(true)
}

pub(crate) fn switch(value: &str) -> Result<bool> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => bail!("expected on/off"),
    }
}
