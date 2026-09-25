use super::{Scope, duration, one, syntax::Directive};
use anyhow::{Context, Result, bail, ensure};
use std::{path::Path, time::Duration};

pub(super) fn apply(s: &mut Scope, n: &Directive, base: &Path, context: &str) -> Result<bool> {
    match n.name.as_str() {
        "ssl_client_certificate" => {
            one(n)?;
            ensure!(context != "location", "client TLS belongs in http/server");
            s.settings
                .security
                .mtls
                .set_ca(std::fs::read_to_string(base.join(&n.args[0]))?)?;
        }
        "ssl_verify_client" => {
            one(n)?;
            ensure!(context != "location", "client TLS belongs in http/server");
            s.settings.security.mtls.mode = match n.args[0].as_str() {
                "off" => crate::security::mtls::Mode::Off,
                "optional" => crate::security::mtls::Mode::Optional,
                "on" => crate::security::mtls::Mode::Required,
                _ => bail!("ssl_verify_client expects on/off/optional"),
            };
        }
        "rgnix_jwt" => {
            if n.args == ["off"] {
                s.settings.security.jwt = None;
            } else {
                let mut spec = crate::security::jwt::Spec {
                    source: String::new(),
                    issuer: String::new(),
                    audience: String::new(),
                    algorithm: "RS256".into(),
                };
                for arg in &n.args {
                    let (key,value)=arg.split_once('=').context("rgnix_jwt expects jwks=FILE_OR_HTTPS_URL issuer=... audience=... [algorithm=RS256|ES256]")?;
                    match key {
                        "jwks" => {
                            spec.source = if value.starts_with("https://") {
                                value.into()
                            } else {
                                base.join(value).to_string_lossy().into()
                            }
                        }
                        "issuer" => spec.issuer = value.into(),
                        "audience" => spec.audience = value.into(),
                        "algorithm" => spec.algorithm = value.into(),
                        _ => bail!("unknown JWT option {key}"),
                    };
                }
                s.settings.security.jwt = Some(crate::security::jwt::Jwt::load(spec)?);
            }
        }
        "rgnix_auth_request" => {
            if n.args == ["off"] {
                s.settings.security.external = None;
            } else {
                ensure!(
                    (1..=3).contains(&n.args.len()),
                    "auth request expects URL [timeout=3s] [headers=x-user,x-tenant]"
                );
                let mut auth = crate::security::External {
                    url: n.args[0].clone(),
                    timeout: Duration::from_secs(3),
                    response_headers: vec![],
                    ..Default::default()
                };
                for arg in &n.args[1..] {
                    if let Some(v) = arg.strip_prefix("timeout=") {
                        auth.timeout = duration(v)?;
                    } else if let Some(v) = arg.strip_prefix("headers=") {
                        auth.response_headers = v.split(',').map(str::to_ascii_lowercase).collect();
                    } else {
                        bail!("unknown auth option {arg}");
                    }
                }
                auth.validate()?;
                s.settings.security.external = Some(auth);
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}
