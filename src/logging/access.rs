use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize)]
pub struct Policy {
    pub json: bool,
    pub query: bool,
    pub client: bool,
    pub referer: bool,
    pub redact: Vec<String>,
    pub fields: Vec<String>,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            json: false,
            query: true,
            client: true,
            referer: true,
            redact: [
                "access_token",
                "token",
                "api_key",
                "apikey",
                "password",
                "secret",
                "authorization",
                "code",
            ]
            .map(str::to_owned)
            .to_vec(),
            fields: vec![],
        }
    }
}
impl Policy {
    pub fn validate_fields(&self) -> Result<()> {
        ensure!(
            self.fields.len() <= 20
                && self.fields.iter().all(|f| [
                    "timestamp",
                    "client",
                    "method",
                    "uri",
                    "protocol",
                    "status",
                    "bytes",
                    "referer",
                    "user_agent",
                    "route",
                    "backend",
                    "upstream",
                    "config",
                    "trace_id",
                    "span_id",
                    "parent_span_id",
                    "upstream_span_id",
                    "trace_sampled",
                    "grpc_status"
                ]
                .contains(&f.as_str())),
            "unsupported access log field"
        );
        ensure!(
            self.redact.len() <= 64 && self.redact.iter().all(|s| !s.is_empty() && s.len() <= 128),
            "invalid query redaction keys"
        );
        Ok(())
    }
    pub fn uri(&self, value: &str) -> String {
        let Some((path, query)) = value.split_once('?') else {
            return value.into();
        };
        if !self.query {
            return path.into();
        }
        let pairs = query
            .split('&')
            .map(|pair| {
                let key = pair.split_once('=').map_or(pair, |p| p.0);
                let decoded = url::form_urlencoded::parse(format!("{key}=").as_bytes())
                    .next()
                    .map(|(k, _)| k.to_ascii_lowercase())
                    .unwrap_or_default();
                if self.redact.iter().any(|r| r.eq_ignore_ascii_case(&decoded)) {
                    format!("{key}=[REDACTED]")
                } else {
                    pair.into()
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        format!("{path}?{pairs}")
    }
    pub fn render(&self, mut value: Value) -> String {
        if let Some(uri) = value["uri"].as_str() {
            value["uri"] = json!(self.uri(uri));
        }
        if let Some(uri) = value["referer"].as_str() {
            value["referer"] = json!(if self.referer {
                self.uri(uri)
            } else {
                "-".into()
            });
        }
        if !self.client {
            value["client"] = json!("-");
        }
        if self.json {
            if !self.fields.is_empty() {
                value
                    .as_object_mut()
                    .unwrap()
                    .retain(|k, _| self.fields.contains(k));
            }
            return value.to_string();
        }
        let text = |key: &str| -> String {
            value[key]
                .as_str()
                .map_or_else(|| value[key].to_string(), str::to_owned)
                .chars()
                .flat_map(|c| match c {
                    '"' => "\\\"".chars().collect::<Vec<_>>(),
                    '\\' => "\\\\".chars().collect(),
                    c if c.is_control() => " ".chars().collect(),
                    c => vec![c],
                })
                .collect()
        };
        format!(
            "{} - - [{}] \"{} {} {}\" {} {} \"{}\" \"{}\" route=\"{}\" backend=\"{}\" upstream=\"{}\" config=\"{}\" trace_id=\"{}\" span_id=\"{}\" parent_span_id=\"{}\" upstream_span_id=\"{}\" trace_sampled={}",
            text("client"),
            text("timestamp"),
            text("method"),
            text("uri"),
            text("protocol"),
            text("status"),
            text("bytes"),
            text("referer"),
            text("user_agent"),
            text("route"),
            text("backend"),
            text("upstream"),
            text("config"),
            text("trace_id"),
            text("span_id"),
            text("parent_span_id"),
            text("upstream_span_id"),
            text("trace_sampled")
        )
    }
}
