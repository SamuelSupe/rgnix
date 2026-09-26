use super::{
    kernel::Loaded,
    maps::{Event, monotonic_ns},
};
use anyhow::Result;
use opentelemetry_proto::tonic::{
    common::v1::{AnyValue, KeyValue, any_value::Value},
    logs::v1::LogRecord,
};
use std::net::{Ipv4Addr, Ipv6Addr};

pub fn export(loaded: &mut Loaded, exporter: Option<&crate::otlp::Exporter>) -> Result<()> {
    for event in loaded.events()? {
        if let Some(exporter) = exporter {
            let now = crate::otlp::trace::now();
            let timestamp = now.saturating_sub(monotonic_ns().saturating_sub(event.timestamp));
            let reason = match event.reason {
                1 => "malformed",
                2 => "unsupported",
                3 => "fragment",
                4 => "rate_budget",
                6 => "rate_contention",
                7 => "state_unavailable",
                _ => "policy",
            };
            let fields = [
                ("event.name", "rgnix.xdp.drop".to_string()),
                ("rgnix.xdp.rule", loaded.rule_name(event.rule)),
                ("rgnix.xdp.reason", reason.into()),
                (
                    "rgnix.xdp.action",
                    if event.observed != 0 {
                        "would_drop"
                    } else {
                        "drop"
                    }
                    .into(),
                ),
                ("rgnix.xdp.revision", loaded.candidate.digest.clone()),
                ("source.address", address(&event, true)),
                ("destination.address", address(&event, false)),
                ("source.port", event.src_port.to_string()),
                ("destination.port", event.dst_port.to_string()),
                ("network.protocol.number", event.protocol.to_string()),
                ("network.packet.size", event.length.to_string()),
            ];
            exporter.network_log(LogRecord {
                time_unix_nano: timestamp,
                observed_time_unix_nano: now,
                severity_number: 9,
                severity_text: "INFO".into(),
                event_name: "rgnix.xdp.drop".into(),
                body: Some(AnyValue {
                    value: Some(Value::StringValue("XDP packet decision".into())),
                }),
                attributes: fields
                    .into_iter()
                    .map(|(key, value)| KeyValue {
                        key: key.into(),
                        value: Some(AnyValue {
                            value: Some(Value::StringValue(value)),
                        }),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            });
        }
    }
    Ok(())
}
fn address(event: &Event, source: bool) -> String {
    let bytes = if source { event.src } else { event.dst };
    match event.version {
        4 => Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[..4]).unwrap()).to_string(),
        6 => Ipv6Addr::from(bytes).to_string(),
        _ => String::new(),
    }
}
