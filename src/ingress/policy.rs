use super::Resources;
use crate::{backend, model::Settings};
use anyhow::{Context, Result, bail, ensure};
use kube::ResourceExt;
use std::{collections::BTreeMap, time::Duration};

pub(super) const PASS: &str = "function on_request() return route.pass() end";

pub(super) fn annotations(values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    values
        .iter()
        .filter(|(k, _)| {
            k.starts_with("rgnix.io/")
                && k.as_str() != "rgnix.io/script"
                && k.as_str() != "rgnix.io/rolled-back-revision"
                && k.as_str() != super::release::PROGRESS
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
pub(super) fn parse(
    values: &BTreeMap<String, String>,
) -> Result<(Settings, backend::Options, bool)> {
    let mut settings = Settings {
        body_policy: crate::body::BodyPolicy::from_annotations(values)?,
        ..Settings::default()
    };
    let mut upstream = backend::Options::default();
    let mut tls = false;
    for (key, value) in values {
        ensure!(value.len() <= 8192, "annotation {key} exceeds 8 KiB");
        let Some(key) = key.strip_prefix("rgnix.io/") else {
            continue;
        };
        match key {
            "script" | "request-body" | "request-body-timeout" | "rolled-back-revision" => {}
            "traffic-policy" => {
                let traffic = crate::rollout::Policy::parse(value)?;
                for service in traffic.services() {
                    auth_service(&format!("{service}/"))?;
                }
            }
            "client-max-body-size" => {
                settings.max_body = crate::config::size(value)?;
                ensure!(
                    settings.max_body <= 1024 * 1024 * 1024 * 1024,
                    "body limit exceeds 1 TiB"
                );
            }
            "proxy-connect-timeout"
            | "proxy-read-timeout"
            | "proxy-send-timeout"
            | "keepalive-timeout" => {
                let time = crate::config::duration(value)?;
                ensure!(
                    time <= Duration::from_secs(86400)
                        && (key == "keepalive-timeout" || !time.is_zero()),
                    "timeout must be positive and at most 24h"
                );
                match key {
                    "proxy-connect-timeout" => settings.connect_timeout = time,
                    "proxy-read-timeout" => settings.read_timeout = time,
                    "proxy-send-timeout" => settings.write_timeout = time,
                    _ => settings.keepalive = time,
                }
            }
            "log-format" => {
                ensure!(
                    matches!(value.as_str(), "combined" | "json"),
                    "log-format expects combined/json"
                );
                settings.log_policy.json = value == "json";
            }
            "log-query" | "log-client" | "log-referer" => {
                ensure!(
                    matches!(value.as_str(), "on" | "off"),
                    "log policy expects on/off"
                );
                match key {
                    "log-query" => settings.log_policy.query = value == "on",
                    "log-client" => settings.log_policy.client = value == "on",
                    _ => settings.log_policy.referer = value == "on",
                }
            }
            "log-redact" | "log-fields" => {
                let names = value.split(',').map(|s| s.trim().to_owned()).collect();
                if key == "log-redact" {
                    settings.log_policy.redact = names;
                } else {
                    settings.log_policy.fields = names;
                }
                settings.log_policy.validate_fields()?;
            }
            "ssl-redirect" => {
                settings.https_redirect_port = match value.as_str() {
                    "true" | "on" => Some(443),
                    "false" | "off" => None,
                    _ => bail!("ssl-redirect expects true/false"),
                };
            }
            "https-port" => {}
            "access-log" => {
                settings.access_log = match value.as_str() {
                    "on" => Some("/dev/stdout".into()),
                    "off" => None,
                    _ => bail!("access-log expects on/off"),
                }
            }
            "backend-protocol" => {
                tls = match value.as_str() {
                    "HTTP" | "GRPC" => false,
                    "HTTPS" | "GRPCS" => true,
                    _ => bail!("backend-protocol expects HTTP, HTTPS, GRPC or GRPCS"),
                };
                if value.starts_with("GRPC") {
                    settings.upstream.protocol = crate::upstream::Protocol::Http2;
                }
            }
            "upstream-http-version" => {
                settings.upstream.protocol = crate::upstream::Protocol::parse(value)?
            }
            "upstream-server-name" => {
                ensure!(
                    !value.is_empty()
                        && value.len() <= 253
                        && !value.contains(['/', ':', '$', ' ']),
                    "invalid upstream server name"
                );
                settings.upstream.server_name = Some(value.clone());
            }
            "upstream-ca-secret" => {
                ensure!(valid_name(value), "CA Secret must be a same-namespace name");
            }
            "jwt-secret" | "client-ca-secret" | "upstream-client-secret" => {
                ensure!(valid_name(value), "Secret must be a same-namespace name");
            }
            "jwt-issuer" | "jwt-audience" => {
                ensure!(
                    !value.is_empty() && value.len() <= 1024,
                    "JWT issuer/audience required and at most 1024 bytes"
                );
            }
            "jwt-algorithm" => {
                ensure!(
                    matches!(value.as_str(), "RS256" | "ES256"),
                    "JWT algorithm must be RS256 or ES256"
                );
            }
            "verify-client" => {
                settings.security.mtls.mode = match value.as_str() {
                    "on" => crate::security::mtls::Mode::Required,
                    "optional" => crate::security::mtls::Mode::Optional,
                    _ => bail!("verify-client expects on or optional"),
                };
            }
            "auth-service" => {
                auth_service(value)?;
            }
            "auth-timeout" => {
                let timeout = crate::config::duration(value)?;
                ensure!(
                    !timeout.is_zero() && timeout <= Duration::from_secs(30),
                    "auth timeout must be 1ms..30s"
                );
            }
            "auth-response-headers" => {
                let auth = crate::security::External {
                    url: "http://auth/".into(),
                    timeout: Duration::from_secs(3),
                    response_headers: value
                        .split(',')
                        .map(|s| s.trim().to_ascii_lowercase())
                        .collect(),
                    ..Default::default()
                };
                auth.validate()?;
            }
            "balance" => {
                upstream.balance = match value.as_str() {
                    "round_robin" => backend::Balance::RoundRobin,
                    "least_conn" => backend::Balance::LeastConnections,
                    _ if value.starts_with("sticky ") => {
                        let cookie = value.trim_start_matches("sticky ");
                        http::header::HeaderName::from_bytes(cookie.as_bytes())?;
                        ensure!(cookie.len() <= 64, "affinity cookie name exceeds 64 bytes");
                        backend::Balance::Sticky(cookie.into())
                    }
                    _ => backend::Balance::Hash(crate::traffic::Key::parse(
                        value
                            .strip_prefix("hash ")
                            .context("balance expects round_robin, least_conn or hash KEY")?,
                    )?),
                };
            }
            "backend-max-inflight" => upstream.max_inflight = value.parse()?,
            "health-path" => {
                upstream.health = Some(backend::HealthCheck {
                    path: value.clone(),
                    interval: Duration::from_secs(10),
                    timeout: Duration::from_secs(1),
                    status: 200,
                })
            }
            "limit-rate" | "limit-conn" => {
                let args: Vec<_> = value.split_whitespace().collect();
                ensure!(!args.is_empty(), "empty traffic policy");
                if value == "off" {
                    continue;
                }
                let limit = args[0].parse()?;
                let mut selector = crate::traffic::Key::Ip;
                let mut burst = limit;
                for arg in &args[1..] {
                    if let Some(v) = arg.strip_prefix("key=") {
                        selector = crate::traffic::Key::parse(v)?;
                    } else if let Some(v) = arg.strip_prefix("burst=") {
                        ensure!(key == "limit-rate", "burst is only valid for rate limits");
                        burst = v.parse()?;
                    } else {
                        bail!("unknown traffic option {arg}");
                    }
                }
                if key == "limit-rate" {
                    settings.traffic.rate = Some(crate::traffic::Rate {
                        per_second: limit,
                        burst,
                        key: selector,
                    });
                } else {
                    settings.traffic.concurrency = Some(crate::traffic::Concurrency {
                        limit,
                        key: selector,
                    });
                }
            }
            "allow-cidrs" => {
                for network in value.split(',') {
                    settings.identity.access.push(crate::identity::AccessRule {
                        network: Some(crate::identity::network(network.trim())?),
                        allow: true,
                    });
                }
                settings.identity.access.push(crate::identity::AccessRule {
                    network: None,
                    allow: false,
                });
            }
            "compression" => {
                match value.as_str() {
                    "off" => {}
                    "gzip" => settings.compression.gzip = 1,
                    "br" => settings.compression.brotli = 4,
                    "gzip br" | "br gzip" => {
                        settings.compression.gzip = 1;
                        settings.compression.brotli = 4;
                    }
                    _ => bail!("compression expects off, gzip, br or gzip br"),
                };
            }
            _ => bail!("unsupported rgnix annotation {key}"),
        }
    }
    if let Some(port) = values.get("rgnix.io/https-port") {
        let port: u16 = port.parse()?;
        ensure!(
            port > 0 && settings.https_redirect_port.is_some(),
            "https-port requires ssl-redirect and a nonzero port"
        );
        settings.https_redirect_port = Some(port);
    }
    settings.traffic.validate()?;
    settings.identity.validate()?;
    upstream.validate()?;
    if values.contains_key("rgnix.io/jwt-secret") {
        ensure!(
            values.contains_key("rgnix.io/jwt-issuer")
                && values.contains_key("rgnix.io/jwt-audience"),
            "JWT requires jwt-issuer and jwt-audience"
        );
    }
    if values.contains_key("rgnix.io/client-ca-secret")
        && !values.contains_key("rgnix.io/verify-client")
    {
        settings.security.mtls.mode = crate::security::mtls::Mode::Required;
    }
    ensure!(
        settings.security.mtls.mode == crate::security::mtls::Mode::Off
            || values.contains_key("rgnix.io/client-ca-secret"),
        "client verification requires client-ca-secret"
    );
    if values
        .get("rgnix.io/backend-protocol")
        .is_some_and(|v| v.starts_with("GRPC"))
    {
        ensure!(
            settings.upstream.protocol == crate::upstream::Protocol::Http2,
            "gRPC requires upstream HTTP/2"
        );
    }
    Ok((settings, upstream, tls))
}

pub(super) fn apply_resources(
    settings: &mut Settings,
    values: &BTreeMap<String, String>,
    resources: &Resources,
    namespace: &str,
) -> Result<()> {
    if let Some(name) = values.get("rgnix.io/upstream-ca-secret") {
        let pem = secret(resources, namespace, name, "ca.crt")?;
        settings.upstream.set_ca(String::from_utf8(pem.to_vec())?)?;
    }
    if let Some(name) = values.get("rgnix.io/upstream-client-secret") {
        settings.upstream.set_identity(
            String::from_utf8(secret(resources, namespace, name, "tls.crt")?.to_vec())?,
            String::from_utf8(secret(resources, namespace, name, "tls.key")?.to_vec())?,
        )?;
    }
    if let Some(name) = values.get("rgnix.io/client-ca-secret") {
        settings.security.mtls.set_ca(String::from_utf8(
            secret(resources, namespace, name, "ca.crt")?.to_vec(),
        )?)?;
    }
    if let Some(name) = values.get("rgnix.io/jwt-secret") {
        let bytes = secret(resources, namespace, name, "jwks.json")?;
        let spec = crate::security::jwt::Spec {
            source: format!("secret:{namespace}/{name}"),
            issuer: values
                .get("rgnix.io/jwt-issuer")
                .cloned()
                .context("JWT issuer missing")?,
            audience: values
                .get("rgnix.io/jwt-audience")
                .cloned()
                .context("JWT audience missing")?,
            algorithm: values
                .get("rgnix.io/jwt-algorithm")
                .cloned()
                .unwrap_or("RS256".into()),
        };
        settings.security.jwt = Some(crate::security::jwt::Jwt::from_bytes(spec, bytes)?);
    }
    if let Some(value) = values.get("rgnix.io/auth-service") {
        let (service, path) = auth_service(value)?;
        let (backend, warning) = super::resolve_backend(resources, namespace, &service);
        ensure!(warning.is_none(), "auth Service unavailable");
        ensure!(
            !backend.endpoints.is_empty(),
            "auth Service has no ready endpoints"
        );
        let endpoints = backend.endpoints.iter().map(|e| e.address).collect();
        let address = backend.endpoints[0].address;
        let auth = crate::security::External {
            url: format!("http://{address}{path}"),
            endpoints,
            pool: Some(std::sync::Arc::new(backend)),
            timeout: values
                .get("rgnix.io/auth-timeout")
                .map(|v| crate::config::duration(v))
                .transpose()?
                .unwrap_or(Duration::from_secs(3)),
            response_headers: values
                .get("rgnix.io/auth-response-headers")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_ascii_lowercase())
                        .collect()
                })
                .unwrap_or_default(),
        };
        auth.validate()?;
        settings.security.external = Some(auth);
    }
    Ok(())
}
pub(super) fn auth_service(
    value: &str,
) -> Result<(
    k8s_openapi::api::networking::v1::IngressServiceBackend,
    String,
)> {
    let (authority, path) = value
        .split_once('/')
        .context("auth-service expects name:port/path")?;
    let (name, port) = authority
        .split_once(':')
        .context("auth-service expects name:port/path")?;
    ensure!(
        valid_name(name)
            && !port.is_empty()
            && !path.contains(['?', '#', '\r', '\n'])
            && path.len() <= 1024,
        "invalid auth Service reference"
    );
    let port = if let Ok(number) = port.parse::<u16>() {
        ensure!(number > 0, "auth port cannot be zero");
        k8s_openapi::api::networking::v1::ServiceBackendPort {
            name: None,
            number: Some(i32::from(number)),
        }
    } else {
        ensure!(valid_name(port), "invalid named auth port");
        k8s_openapi::api::networking::v1::ServiceBackendPort {
            name: Some(port.into()),
            number: None,
        }
    };
    Ok((
        k8s_openapi::api::networking::v1::IngressServiceBackend {
            name: name.into(),
            port: Some(port),
        },
        format!("/{path}"),
    ))
}
pub(super) fn dependencies(
    values: &BTreeMap<String, String>,
    namespace: &str,
    services: &mut std::collections::BTreeSet<(String, String)>,
    secrets: &mut std::collections::BTreeSet<(String, String)>,
) {
    if let Some(value) = values.get("rgnix.io/traffic-policy")
        && let Ok(policy) = crate::rollout::Policy::parse(value)
    {
        for reference in policy.services() {
            if let Ok((backend, _)) = auth_service(&format!("{reference}/")) {
                services.insert((namespace.into(), backend.name));
            }
        }
    }
    for key in [
        "upstream-ca-secret",
        "upstream-client-secret",
        "client-ca-secret",
        "jwt-secret",
    ] {
        if let Some(name) = values.get(&format!("rgnix.io/{key}")) {
            secrets.insert((namespace.into(), name.clone()));
        }
    }
    if let Some(value) = values.get("rgnix.io/auth-service")
        && let Ok((backend, _)) = auth_service(value)
    {
        services.insert((namespace.into(), backend.name));
    }
}
pub(super) fn secret<'a>(
    resources: &'a Resources,
    namespace: &str,
    name: &str,
    key: &str,
) -> Result<&'a [u8]> {
    resources
        .secrets
        .iter()
        .find(|s| s.namespace().as_deref() == Some(namespace) && s.name_any() == name)
        .and_then(|s| s.data.as_ref()?.get(key))
        .map(|v| v.0.as_slice())
        .with_context(|| format!("Secret {namespace}/{name} key {key} unavailable"))
}
pub(super) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.')
}
