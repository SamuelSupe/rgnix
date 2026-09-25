use super::{
    Exporter,
    record::{attribute, string_attribute},
};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::trace::v1::{Span, Status, span::SpanKind, status::StatusCode};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Trace {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent: [u8; 8],
    pub upstream: [u8; 8],
    pub sampled: bool,
    pub enabled: bool,
    pub start: u64,
    pub upstream_start: Option<u64>,
}
impl Trace {
    pub fn new(headers: &http::HeaderMap, ratio: Option<f64>) -> Option<Self> {
        let parent = headers
            .get("traceparent")
            .and_then(|v| v.to_str().ok())
            .and_then(parse);
        if ratio.is_none() && parent.is_none() {
            return None;
        }
        let (trace_id, parent_id, inherited) =
            parent.unwrap_or_else(|| (rand::random(), [0; 8], false));
        let enabled = ratio.is_some();
        let sampled = enabled
            && if parent.is_some() {
                inherited
            } else {
                rand::random::<f64>() < ratio.unwrap_or(0.0)
            };
        Some(Self {
            trace_id,
            span_id: if enabled { rand::random() } else { parent_id },
            parent: parent_id,
            upstream: rand::random(),
            sampled,
            enabled,
            start: now(),
            upstream_start: None,
        })
    }
    pub fn header(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            hex(&self.trace_id),
            hex(&self.upstream),
            u8::from(self.sampled)
        )
    }
    pub fn export(
        &self,
        exporter: &Exporter,
        route: &str,
        method: &str,
        result: (u16, Option<u16>),
        backend: Option<&str>,
        upstream_error: bool,
    ) {
        let (status, grpc_status) = result;
        if !self.enabled || !self.sampled {
            return;
        }
        let end = now();
        let mut attributes = vec![
            string_attribute("http.request.method", method),
            string_attribute("http.route", route),
            attribute(
                "http.response.status_code",
                Value::IntValue(i64::from(status)),
            ),
        ];
        if let Some(code) = grpc_status {
            attributes.push(attribute(
                "rpc.grpc.status_code",
                Value::IntValue(i64::from(code)),
            ));
        }
        let failed = upstream_error || status >= 500 || grpc_status.is_some_and(|code| code != 0);
        exporter.span(Span {
            trace_id: self.trace_id.to_vec(),
            span_id: self.span_id.to_vec(),
            parent_span_id: if self.parent == [0; 8] {
                vec![]
            } else {
                self.parent.to_vec()
            },
            name: format!("{method} {route}"),
            kind: SpanKind::Server as i32,
            start_time_unix_nano: self.start,
            end_time_unix_nano: end,
            attributes: attributes.clone(),
            flags: 1,
            status: Some(Status {
                code: if failed {
                    StatusCode::Error
                } else {
                    StatusCode::Unset
                } as i32,
                message: String::new(),
            }),
            ..Default::default()
        });
        if let (Some(start), Some(backend)) = (self.upstream_start, backend) {
            exporter.span(Span {
                trace_id: self.trace_id.to_vec(),
                span_id: self.upstream.to_vec(),
                parent_span_id: self.span_id.to_vec(),
                name: format!("{method} upstream"),
                kind: SpanKind::Client as i32,
                start_time_unix_nano: start,
                end_time_unix_nano: end,
                attributes: {
                    attributes.push(string_attribute("rgnix.upstream.name", backend));
                    attributes
                },
                flags: 1,
                status: Some(Status {
                    code: if failed {
                        StatusCode::Error
                    } else {
                        StatusCode::Unset
                    } as i32,
                    message: String::new(),
                }),
                ..Default::default()
            });
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
fn parse(value: &str) -> Option<([u8; 16], [u8; 8], bool)> {
    if value.len() != 55 {
        return None;
    }
    let parts: Vec<_> = value.split('-').collect();
    if parts.len() != 4
        || parts[0] != "00"
        || parts[1].len() != 32
        || parts[2].len() != 16
        || parts[3].len() != 2
    {
        return None;
    }
    fn decode<const N: usize>(value: &str) -> Option<[u8; N]> {
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let mut output = [0; N];
        for (i, byte) in output.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(output)
    }
    let trace = decode(parts[1])?;
    let span = decode(parts[2])?;
    if trace == [0; 16] || span == [0; 8] {
        return None;
    }
    Some((trace, span, decode::<1>(parts[3])?[0] & 1 == 1))
}
