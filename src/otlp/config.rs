use anyhow::{Context, Result, ensure};
use opentelemetry_proto::tonic::resource::v1::Resource;
use reqwest::{
    Client, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use std::{collections::BTreeMap, env, time::Duration};

#[derive(Clone, clap::Args)]
pub struct Options {
    /// Export access logs to this complete OTLP/HTTP protobuf URL (including /v1/logs).
    #[arg(long, env = "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", hide_env_values = true)]
    pub otlp_logs_endpoint: Option<String>,
    /// Export proxy spans to this complete OTLP/HTTP protobuf URL (including /v1/traces).
    #[arg(
        long,
        env = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        hide_env_values = true
    )]
    pub otlp_traces_endpoint: Option<String>,
    #[arg(long, env = "RGNIX_TRACE_SAMPLE_RATIO", default_value_t = 0.1)]
    pub trace_sample_ratio: f64,
}

#[derive(Clone, Copy)]
pub(super) enum Signal {
    Logs,
    Traces,
}
impl Signal {
    pub fn name(self) -> &'static str {
        match self {
            Self::Logs => "logs",
            Self::Traces => "traces",
        }
    }
}
pub(super) struct Config {
    pub signal: Signal,
    pub endpoint: Url,
    pub client: Client,
    pub resource: Resource,
    pub capacity: usize,
    pub batch_size: usize,
    pub interval: Duration,
    pub timeout: Duration,
}

impl Options {
    pub(super) fn configure(self, signal: Signal) -> Result<Option<Config>> {
        let name = signal.name();
        let upper = name.to_ascii_uppercase();
        let signal_variable = |suffix: &str| -> Result<Option<String>> {
            variable(&format!("OTEL_EXPORTER_OTLP_{upper}_{suffix}"))?.map_or_else(
                || variable(&format!("OTEL_EXPORTER_OTLP_{suffix}")),
                |v| Ok(Some(v)),
            )
        };
        let exporter = format!("OTEL_{upper}_EXPORTER");
        let batch_prefix = if matches!(signal, Signal::Logs) {
            "OTEL_BLRP"
        } else {
            "OTEL_BSP"
        };
        if variable("OTEL_SDK_DISABLED")?.as_deref() == Some("true")
            || variable(&exporter)?.as_deref() == Some("none")
        {
            return Ok(None);
        }
        let explicit = match signal {
            Signal::Logs => self.otlp_logs_endpoint,
            Signal::Traces => self.otlp_traces_endpoint,
        };
        if matches!(signal, Signal::Traces)
            && explicit.is_none()
            && variable(&exporter)?.as_deref() != Some("otlp")
        {
            return Ok(None);
        }
        let endpoint = if let Some(endpoint) = explicit.filter(|v| !v.is_empty()) {
            endpoint_url(&endpoint)?
        } else if let Some(base) = variable("OTEL_EXPORTER_OTLP_ENDPOINT")? {
            let mut url = endpoint_url(&base)?;
            url.set_path(&format!("{}/v1/{name}", url.path().trim_end_matches('/')));
            url
        } else {
            return Ok(None);
        };
        ensure!(
            variable(&exporter)?.is_none_or(|v| v == "otlp"),
            "{exporter} must be otlp or none"
        );
        ensure!(
            signal_variable("PROTOCOL")?.is_none_or(|v| v == "http/protobuf"),
            "OTLP {name} supports only http/protobuf; use a Collector for other transports"
        );
        let timeout = Duration::from_millis(number(
            signal_variable("TIMEOUT")?,
            &format!("OTEL_EXPORTER_OTLP_{upper}_TIMEOUT"),
            5000,
            1,
            30000,
        )? as u64);
        let mut headers = HeaderMap::new();
        for (name, value) in pairs(signal_variable("HEADERS")?, "OTLP headers", 8192)? {
            let name =
                HeaderName::from_bytes(name.as_bytes()).context("invalid OTLP header name")?;
            ensure!(
                !matches!(
                    name.as_str(),
                    "host"
                        | "content-type"
                        | "content-length"
                        | "transfer-encoding"
                        | "connection"
                        | "content-encoding"
                        | "accept-encoding"
                ),
                "OTLP headers must not override HTTP framing or encoding"
            );
            let mut value = HeaderValue::from_str(&value).context("invalid OTLP header value")?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let mut builder = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(timeout)
            .timeout(timeout)
            .user_agent(concat!("rgnix/", env!("CARGO_PKG_VERSION")));
        if let Some(path) = signal_variable("CERTIFICATE")? {
            let mut file =
                std::fs::File::open(path).context("cannot open OTLP CA certificate file")?;
            use std::io::Read;
            let mut pem = Vec::new();
            (&mut file).take(1024 * 1024 + 1).read_to_end(&mut pem)?;
            ensure!(pem.len() <= 1024 * 1024, "OTLP CA file exceeds 1 MiB");
            let certificates = reqwest::Certificate::from_pem_bundle(&pem)
                .context("invalid OTLP CA certificate bundle")?;
            ensure!(
                !certificates.is_empty(),
                "OTLP CA bundle has no certificates"
            );
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let mut resource = pairs(
            variable("OTEL_RESOURCE_ATTRIBUTES")?,
            "OTEL_RESOURCE_ATTRIBUTES",
            1024,
        )?;
        resource
            .entry("service.name".into())
            .or_insert_with(|| "rgnix".into());
        resource
            .entry("service.version".into())
            .or_insert_with(|| env!("CARGO_PKG_VERSION").into());
        if let Some(name) = variable("OTEL_SERVICE_NAME")? {
            ensure!(name.len() <= 1024, "OTEL_SERVICE_NAME exceeds 1024 bytes");
            resource.insert("service.name".into(), name);
        }
        let capacity = number(
            variable(&format!("{batch_prefix}_MAX_QUEUE_SIZE"))?,
            &format!("{batch_prefix}_MAX_QUEUE_SIZE"),
            2048,
            1,
            16384,
        )?;
        let batch_size = number(
            variable(&format!("{batch_prefix}_MAX_EXPORT_BATCH_SIZE"))?,
            &format!("{batch_prefix}_MAX_EXPORT_BATCH_SIZE"),
            256,
            1,
            512,
        )?;
        ensure!(
            batch_size <= capacity,
            "OTLP batch size must not exceed queue size"
        );
        let interval = Duration::from_millis(number(
            variable(&format!("{batch_prefix}_SCHEDULE_DELAY"))?,
            &format!("{batch_prefix}_SCHEDULE_DELAY"),
            1000,
            1,
            60000,
        )? as u64);
        Ok(Some(Config {
            signal,
            endpoint,
            client: builder
                .build()
                .context("cannot initialize OTLP HTTPS client")?,
            resource: Resource {
                attributes: resource
                    .into_iter()
                    .map(|(k, v)| super::record::string_attribute(&k, &v))
                    .collect(),
                ..Default::default()
            },
            capacity,
            batch_size,
            interval,
            timeout,
        }))
    }
}

fn endpoint_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("invalid OTLP endpoint URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "OTLP endpoint requires http(s), a host, no userinfo and no fragment"
    );
    Ok(url)
}

fn variable(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(_) => anyhow::bail!("{name} must be UTF-8"),
    }
}
fn number(
    value: Option<String>,
    name: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize> {
    let value = match value {
        Some(value) => value
            .parse()
            .with_context(|| format!("{name} must be an integer"))?,
        None => default,
    };
    ensure!((min..=max).contains(&value), "{name} must be {min}..{max}");
    Ok(value)
}
fn pairs(value: Option<String>, name: &str, max_value: usize) -> Result<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    if let Some(value) = value {
        ensure!(value.len() <= 16384, "{name} exceeds 16 KiB");
        for (index, item) in value.split(',').enumerate() {
            ensure!(index < 32, "{name} exceeds 32 entries");
            let (key, value) = item
                .split_once('=')
                .with_context(|| format!("{name} requires key=value pairs"))?;
            let key = key.trim();
            let value = percent_encoding::percent_decode_str(value.trim())
                .decode_utf8()
                .with_context(|| format!("{name} contains invalid UTF-8"))?
                .into_owned();
            ensure!(
                !key.is_empty() && key.len() <= 128 && value.len() <= max_value,
                "{name} key/value exceeds allowed length"
            );
            entries.insert(key.into(), value);
        }
    }
    Ok(entries)
}
