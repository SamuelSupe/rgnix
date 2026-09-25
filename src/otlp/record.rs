use opentelemetry_proto::tonic::{
    common::v1::{AnyValue, KeyValue, any_value::Value},
    logs::v1::{LogRecord, SeverityNumber},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Request-completion fields; no query, body, cookies or authorization headers are exported.
pub struct AccessRecord<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub host: &'a str,
    pub client: &'a str,
    pub protocol_version: &'a str,
    pub tls: bool,
    pub status: u16,
    pub response_bytes: usize,
    pub duration: Duration,
    pub route: &'a str,
    pub backend: Option<&'a str>,
    pub upstream: Option<std::net::SocketAddr>,
    pub config_hash: &'a str,
    pub config_version: u64,
    pub error_source: Option<&'a str>,
    pub trace: Option<&'a super::trace::Trace>,
}

impl AccessRecord<'_> {
    pub(super) fn into_log(self) -> LogRecord {
        let mut attributes = vec![
            string_attribute("http.request.method", self.method),
            string_attribute("url.path", truncate(self.path, 4096)),
            string_attribute("url.scheme", if self.tls { "https" } else { "http" }),
            string_attribute("server.address", self.host),
            string_attribute("client.address", self.client),
            string_attribute("network.protocol.name", "http"),
            string_attribute("network.protocol.version", self.protocol_version),
            attribute(
                "http.response.status_code",
                Value::IntValue(self.status.into()),
            ),
            attribute(
                "http.response.body.size",
                Value::IntValue(self.response_bytes.min(i64::MAX as usize) as i64),
            ),
            attribute(
                "rgnix.request.duration_ms",
                Value::DoubleValue(self.duration.as_secs_f64() * 1000.0),
            ),
            string_attribute("rgnix.route.id", self.route),
            string_attribute("rgnix.config.sha256", self.config_hash),
            attribute(
                "rgnix.config.version",
                Value::IntValue(self.config_version.min(i64::MAX as u64) as i64),
            ),
        ];
        if let Some(backend) = self.backend {
            attributes.push(string_attribute("rgnix.upstream.name", backend));
        }
        if let Some(upstream) = self.upstream {
            attributes.push(string_attribute(
                "rgnix.upstream.address",
                &upstream.to_string(),
            ));
        }
        if let Some(source) = self.error_source {
            attributes.push(string_attribute("rgnix.error.source", source));
        }
        let severity = if self.status >= 500 || self.error_source.is_some() {
            SeverityNumber::Error
        } else if self.status >= 400 {
            SeverityNumber::Warn
        } else {
            SeverityNumber::Info
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        LogRecord {
            time_unix_nano: now,
            observed_time_unix_nano: now,
            severity_number: severity as i32,
            severity_text: match severity {
                SeverityNumber::Error => "ERROR",
                SeverityNumber::Warn => "WARN",
                _ => "INFO",
            }
            .into(),
            body: Some(AnyValue {
                value: Some(Value::StringValue("HTTP access".into())),
            }),
            attributes,
            trace_id: self.trace.map_or_else(Vec::new, |t| t.trace_id.to_vec()),
            span_id: self.trace.map_or_else(Vec::new, |t| t.span_id.to_vec()),
            flags: self.trace.map_or(0, |t| u32::from(t.sampled)),
            ..Default::default()
        }
    }
}

fn truncate(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}
pub(super) fn string_attribute(key: &str, value: &str) -> KeyValue {
    attribute(
        key,
        Value::StringValue(truncate(value, if key == "url.path" { 4096 } else { 1024 }).into()),
    )
}
pub(super) fn attribute(key: &str, value: Value) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue { value: Some(value) }),
        ..Default::default()
    }
}
