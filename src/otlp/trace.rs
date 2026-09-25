use super::{
    Exporter, propagation,
    record::{attribute, string_attribute},
};
use opentelemetry_proto::tonic::{
    common::v1::{KeyValue, any_value::Value},
    trace::v1::{Span, SpanFlags, Status, span::SpanKind, status::StatusCode},
};
use rand::RngCore;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Trace {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent: [u8; 8],
    pub sampled: bool,
    pub enabled: bool,
    start: u64,
    state: String,
    forwarded_parent: Option<String>,
    pub upstream: Option<ClientSpan>,
}

pub struct ClientSpan {
    span: Span,
    enabled: bool,
    forwarded_parent: Option<String>,
    pub status: Option<u16>,
}

impl Trace {
    pub fn new(headers: &http::HeaderMap, ratio: Option<f64>) -> Option<Self> {
        let parent = propagation::extract(headers);
        if ratio.is_none() && parent.is_none() {
            return None;
        }
        let trace_id = parent.as_ref().map_or_else(random_id, |p| p.trace);
        let parent_id = parent.as_ref().map_or([0; 8], |p| p.parent);
        let enabled = ratio.is_some();
        let sampled = parent.as_ref().map_or_else(
            || rand::random::<f64>() < ratio.unwrap_or(0.0),
            |p| p.sampled,
        );
        Some(Self {
            trace_id,
            span_id: if enabled { random_id() } else { parent_id },
            parent: parent_id,
            sampled,
            enabled,
            start: now(),
            state: parent
                .as_ref()
                .map_or_else(String::new, |p| p.state.clone()),
            forwarded_parent: parent.filter(|_| !enabled).map(|p| p.header),
            upstream: None,
        })
    }

    pub fn client(&self, method: &str, backend: &str, role: &str) -> ClientSpan {
        let id = if self.enabled {
            random_id()
        } else {
            self.span_id
        };
        let mut attributes = method_attributes(method);
        attributes.push(string_attribute("rgnix.upstream.name", backend));
        attributes.push(string_attribute("rgnix.upstream.role", role));
        ClientSpan {
            span: Span {
                trace_id: self.trace_id.to_vec(),
                span_id: id.to_vec(),
                parent_span_id: self.span_id.to_vec(),
                trace_state: self.state.clone(),
                flags: u32::from(self.sampled) | SpanFlags::ContextHasIsRemoteMask as u32,
                name: format!("{} {role}", known_method(method)),
                kind: SpanKind::Client as i32,
                start_time_unix_nano: now(),
                attributes,
                ..Default::default()
            },
            enabled: self.enabled,
            forwarded_parent: self.forwarded_parent.clone(),
            status: None,
        }
    }

    pub fn export(
        &self,
        exporter: &Exporter,
        route: (&str, Option<&str>),
        method: &str,
        result: (u16, Option<u16>),
        error: Option<&pingora::Error>,
    ) {
        if !self.enabled || !self.sampled {
            return;
        }
        let (status, grpc_status) = result;
        let mut attributes = method_attributes(method);
        attributes.push(string_attribute("rgnix.route.id", route.0));
        if let Some(path) = route.1 {
            attributes.push(string_attribute("http.route", path));
        }
        attributes.push(attribute(
            "http.response.status_code",
            Value::IntValue(status.into()),
        ));
        grpc_attribute(&mut attributes, grpc_status);
        let transport_error = error
            .filter(|e| !matches!(e.etype(), pingora::ErrorType::HTTPStatus(_)))
            .map(crate::telemetry::traffic::error_reason);
        let failure = transport_error.map(str::to_owned).or_else(|| {
            if status >= 500 {
                Some(status.to_string())
            } else {
                grpc_error(grpc_status)
            }
        });
        let span_status = span_status(&mut attributes, failure);
        if let Some(client) = &self.upstream {
            client.export(
                exporter,
                error
                    .filter(|e| e.esource() == &pingora::ErrorSource::Upstream)
                    .map(crate::telemetry::traffic::error_reason),
                grpc_status,
            );
        }
        exporter.span(Span {
            trace_id: self.trace_id.to_vec(),
            span_id: self.span_id.to_vec(),
            parent_span_id: if self.parent == [0; 8] {
                vec![]
            } else {
                self.parent.to_vec()
            },
            trace_state: self.state.clone(),
            flags: u32::from(self.sampled)
                | if self.parent == [0; 8] {
                    0
                } else {
                    SpanFlags::ContextHasIsRemoteMask as u32 | SpanFlags::ContextIsRemoteMask as u32
                },
            name: route.1.map_or_else(
                || known_method(method).into(),
                |p| format!("{} {p}", known_method(method)),
            ),
            kind: SpanKind::Server as i32,
            start_time_unix_nano: self.start,
            end_time_unix_nano: now().max(self.start),
            attributes,
            status: Some(span_status),
            ..Default::default()
        });
    }
}

impl ClientSpan {
    pub fn id(&self) -> String {
        hex(&self.span.span_id)
    }

    pub fn inject(&self, headers: &mut http::HeaderMap) {
        let parent = self.forwarded_parent.clone().unwrap_or_else(|| {
            format!(
                "00-{}-{}-{:02x}",
                hex(&self.span.trace_id),
                self.id(),
                self.span.flags & 1
            )
        });
        headers.insert(
            "traceparent",
            parent.parse().expect("validated traceparent"),
        );
        headers.remove("tracestate");
        if !self.span.trace_state.is_empty() {
            headers.insert(
                "tracestate",
                self.span.trace_state.parse().expect("validated tracestate"),
            );
        }
    }

    pub fn export(&self, exporter: &Exporter, error: Option<&str>, grpc_status: Option<u16>) {
        if !self.enabled || self.span.flags & 1 == 0 {
            return;
        }
        let mut span = self.span.clone();
        span.end_time_unix_nano = now().max(span.start_time_unix_nano);
        if let Some(status) = self.status {
            span.attributes.push(attribute(
                "http.response.status_code",
                Value::IntValue(status.into()),
            ));
        }
        grpc_attribute(&mut span.attributes, grpc_status);
        let failure = error
            .map(str::to_owned)
            .or_else(|| self.status.filter(|s| *s >= 400).map(|s| s.to_string()))
            .or_else(|| grpc_error(grpc_status));
        span.status = Some(span_status(&mut span.attributes, failure));
        exporter.span(span);
    }
}

fn known_method(method: &str) -> &str {
    if [
        "CONNECT", "DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT", "TRACE",
    ]
    .contains(&method)
    {
        method
    } else {
        "_OTHER"
    }
}
fn method_attributes(method: &str) -> Vec<KeyValue> {
    let mut attributes = vec![string_attribute(
        "http.request.method",
        known_method(method),
    )];
    if known_method(method) == "_OTHER" {
        attributes.push(string_attribute("http.request.method_original", method));
    }
    attributes
}
fn grpc_attribute(attributes: &mut Vec<KeyValue>, status: Option<u16>) {
    if let Some(code) = status {
        attributes.push(attribute(
            "rpc.grpc.status_code",
            Value::IntValue(code.into()),
        ));
    }
}
fn grpc_error(status: Option<u16>) -> Option<String> {
    status.filter(|s| *s != 0).map(|s| format!("grpc_{s}"))
}
fn span_status(attributes: &mut Vec<KeyValue>, error: Option<String>) -> Status {
    let failed = error.is_some();
    if let Some(reason) = error {
        attributes.push(string_attribute("error.type", &reason));
    }
    Status {
        code: if failed {
            StatusCode::Error
        } else {
            StatusCode::Unset
        } as i32,
        message: String::new(),
    }
}
fn random_id<const N: usize>() -> [u8; N] {
    let mut id = [0; N];
    loop {
        rand::thread_rng().fill_bytes(&mut id);
        if id != [0; N] {
            return id;
        }
    }
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
