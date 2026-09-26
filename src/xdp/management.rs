use super::{config, service::State};
use anyhow::Result;
use prometheus::Encoder;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};

pub async fn serve(listener: TcpListener, state: Arc<Mutex<State>>) -> Result<()> {
    let permits = Arc::new(Semaphore::new(32));
    loop {
        let (stream, _) = listener.accept().await?;
        if let Ok(permit) = permits.clone().try_acquire_owned() {
            let state = state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = tokio::time::timeout(Duration::from_secs(2), respond(stream, state)).await;
            });
        }
    }
}
async fn respond(mut stream: TcpStream, state: Arc<Mutex<State>>) -> Result<()> {
    let mut request = Vec::new();
    let mut buf = [0; 1024];
    while request.len() < 8192 && !request.windows(4).any(|s| s == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buf[..n]);
    }
    let first = request.split(|b| *b == b'\n').next().unwrap_or_default();
    let (status, content_type, body) = {
        let state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("XDP state poisoned"))?;
        match first {
            b"GET /healthz HTTP/1.1\r" => (200, "text/plain", "ok\n".into()),
            b"GET /readyz HTTP/1.1\r" => {
                if state.last_error.is_none()
                    && state.desired == state.loaded.candidate.input_digest
                {
                    (200, "text/plain", "ok\n".into())
                } else {
                    (
                        503,
                        "text/plain",
                        "active policy retained; desired revision not applied\n".into(),
                    )
                }
            }
            b"GET /status HTTP/1.1\r" => (
                200,
                "application/json",
                serde_json::to_string_pretty(&serde_json::json!({
                    "abi": config::ABI, "interface": state.interface, "mode": format!("{:?}", state.mode).to_lowercase(), "persistent": state.persistent,
                    "desired_revision": state.desired, "applied_revision": state.loaded.candidate.input_digest,
                    "policy_revision": state.loaded.candidate.digest, "object_sha256": state.loaded.candidate.object_digest,
                    "revision": state.loaded.candidate.config.revision, "observe": state.loaded.candidate.config.observe,
                    "generation": state.generation, "applied_at": state.applied_at, "last_error": state.last_error, "history_error": state.history_error,
                    "rate_entries": state.rate_entries, "rate_capacity": config::RATE_CAPACITY, "rules": state.rules()?
                }))?,
            ),
            b"GET /metrics HTTP/1.1\r" => (200, "text/plain; version=0.0.4", metrics(&state)?),
            _ => (404, "text/plain", "not found\n".into()),
        }
    };
    stream.write_all(format!("HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", if status == 200 { "OK" } else if status == 503 { "Service Unavailable" } else { "Not Found" }, body.len()).as_bytes()).await?;
    Ok(())
}
fn metrics(state: &State) -> Result<String> {
    let mut stats = state.loaded.all_stats()?;
    for (value, previous) in stats.iter_mut().zip(state.previous) {
        *value = value.saturating_add(previous);
    }
    let mut text = String::new();
    for (name, help, ty) in [
        ("packets_total", "Actual XDP packet decisions", "counter"),
        (
            "rule_packets_total",
            "Rule verdicts including observation mode",
            "counter",
        ),
        (
            "rule_bytes_total",
            "Ethernet bytes matched by rule verdict",
            "counter",
        ),
    ] {
        text.push_str(&format!(
            "# HELP rgnix_xdp_{name} {help}\n# TYPE rgnix_xdp_{name} {ty}\n"
        ));
    }
    for (i, action) in ["pass", "drop"].into_iter().enumerate() {
        text.push_str(&format!(
            "rgnix_xdp_packets_total{{action=\"{action}\"}} {}\n",
            stats[i]
        ));
    }
    for row in state.rules()? {
        for (action, count) in [
            ("pass", row.pass),
            ("drop", row.drop),
            ("would_drop", row.would_drop),
        ] {
            text.push_str(&format!("rgnix_xdp_rule_packets_total{{rule=\"{}\",action=\"{action}\"}} {}\nrgnix_xdp_rule_bytes_total{{rule=\"{}\",action=\"{action}\"}} {}\n", row.rule, count.packets, row.rule, count.bytes));
        }
    }
    for (name, value, ty, help) in [
        (
            "malformed_total",
            stats[2],
            "counter",
            "Malformed packets within configured scope",
        ),
        (
            "rate_denied_total",
            stats[3],
            "counter",
            "Rate budget denials",
        ),
        (
            "would_drop_total",
            stats[4],
            "counter",
            "Packets passed by observation mode",
        ),
        (
            "scope_bypass_total",
            stats[5],
            "counter",
            "Packets outside the configured scope",
        ),
        (
            "rate_contention_total",
            stats[6],
            "counter",
            "Rate checks denied due to atomic contention",
        ),
        (
            "rate_state_errors_total",
            stats[7],
            "counter",
            "Rate checks denied because state was unavailable",
        ),
        (
            "rate_insertions_total",
            stats[8],
            "counter",
            "New keyed buckets including LRU replacement",
        ),
        (
            "events_lost_total",
            stats[9],
            "counter",
            "Samples discarded by event budget or full ring",
        ),
        (
            "config_generation",
            state.generation,
            "gauge",
            "Successful policy generations in this process",
        ),
        (
            "reload_failures_total",
            state.reload_failures,
            "counter",
            "Rejected policy updates",
        ),
        (
            "rate_entries",
            state.rate_entries as u64,
            "gauge",
            "Keyed state entries sampled every 30 seconds",
        ),
        (
            "rate_capacity",
            config::RATE_CAPACITY as u64,
            "gauge",
            "Maximum keyed state entries",
        ),
        (
            "config_converged",
            u64::from(
                state.last_error.is_none() && state.desired == state.loaded.candidate.input_digest,
            ),
            "gauge",
            "Desired revision has been applied",
        ),
        (
            "last_apply_timestamp_seconds",
            state.applied_at,
            "gauge",
            "Last successful publication time",
        ),
        (
            "observe",
            u64::from(state.loaded.candidate.config.observe),
            "gauge",
            "Policy is in observation mode",
        ),
        (
            "history_errors",
            u64::from(state.history_error.is_some()),
            "gauge",
            "Last publication could not be saved to history",
        ),
    ] {
        text.push_str(&format!("# HELP rgnix_xdp_{name} {help}\n# TYPE rgnix_xdp_{name} {ty}\nrgnix_xdp_{name} {value}\n"));
    }
    let mut encoded = Vec::new();
    prometheus::TextEncoder::new().encode(&state.registry.gather(), &mut encoded)?;
    text.push_str(std::str::from_utf8(&encoded)?);
    Ok(text)
}
