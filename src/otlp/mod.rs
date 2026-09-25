mod config;
mod propagation;
mod record;
pub mod trace;
pub use config::Options;
pub use record::AccessRecord;

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use bytes::Bytes;
use opentelemetry_proto::tonic::{
    collector::{
        logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse},
        trace::v1::ExportTraceServiceRequest,
    },
    common::v1::InstrumentationScope,
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    trace::v1::{ResourceSpans, ScopeSpans, Span},
};
use prometheus::{IntCounter, IntCounterVec, IntGauge, Registry};
use prost::Message;
use std::{
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, SystemTime},
};
use tokio::sync::{mpsc, oneshot};

const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_BATCH_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Metrics {
    exported: IntCounter,
    dropped: IntCounterVec,
    errors: IntCounter,
    retries: IntCounter,
    partial: IntCounter,
    pending: IntGauge,
}
impl Metrics {
    fn register(registry: &Registry, signal: config::Signal) -> Result<Self> {
        let name = signal.name();
        let exported = IntCounter::new(
            format!("rgnix_otlp_{name}_exported_total"),
            "Access records accepted by the OTLP receiver",
        )?;
        let dropped = IntCounterVec::new(
            prometheus::Opts::new(
                format!("rgnix_otlp_{name}_dropped_total"),
                "Access records lost before or during OTLP export",
            ),
            &["reason"],
        )?;
        let errors = IntCounter::new(
            format!("rgnix_otlp_{name}_export_errors_total"),
            "Failed OTLP attempts and export deadlines",
        )?;
        let retries = IntCounter::new(
            format!("rgnix_otlp_{name}_retries_total"),
            "OTLP export retries",
        )?;
        let partial = IntCounter::new(
            format!("rgnix_otlp_{name}_partial_success_total"),
            "OTLP responses with partial success or warnings",
        )?;
        let pending = IntGauge::new(
            format!("rgnix_otlp_{name}_pending"),
            "OTLP records queued or being exported",
        )?;
        for counter in [&exported, &errors, &retries, &partial] {
            registry.register(Box::new(counter.clone()))?;
        }
        registry.register(Box::new(dropped.clone()))?;
        registry.register(Box::new(pending.clone()))?;
        Ok(Self {
            exported,
            dropped,
            errors,
            retries,
            partial,
            pending,
        })
    }
    fn drop_records(&self, count: usize, reason: &str) {
        self.dropped
            .with_label_values(&[reason])
            .inc_by(count as u64);
    }
}

enum Record {
    Log(LogRecord),
    Span(Span),
}
impl Record {
    fn encoded_len(&self) -> usize {
        match self {
            Self::Log(record) => record.encoded_len(),
            Self::Span(record) => record.encoded_len(),
        }
    }
}
pub struct Exporter {
    sender: ArcSwapOption<mpsc::Sender<Record>>,
    metrics: Metrics,
    worker: Mutex<Option<(oneshot::Sender<()>, JoinHandle<()>)>>,
}
impl Exporter {
    pub fn start(options: Options, registry: &Registry) -> Result<Option<Self>> {
        Self::start_signal(options, registry, config::Signal::Logs)
    }
    pub fn start_traces(options: Options, registry: &Registry) -> Result<Option<Self>> {
        Self::start_signal(options, registry, config::Signal::Traces)
    }
    fn start_signal(
        options: Options,
        registry: &Registry,
        signal: config::Signal,
    ) -> Result<Option<Self>> {
        let Some(config) = options.configure(signal)? else {
            return Ok(None);
        };
        let metrics = Metrics::register(registry, signal)?;
        let (sender, receiver) = mpsc::channel(config.capacity);
        let (stop, stopping) = oneshot::channel();
        let worker_metrics = metrics.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let worker = std::thread::Builder::new()
            .name(format!("otlp-{}",signal.name()))
            .spawn(move || {
                runtime.block_on(async {
                    let work = run(config, receiver, &worker_metrics);
                    tokio::pin!(work);
                    tokio::select! {
                        _ = &mut work => {},
                        _ = stopping => {
                            if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut work).await.is_err() {
                                let remaining = worker_metrics.pending.get().max(0) as usize;
                                worker_metrics.drop_records(remaining, "shutdown");
                                worker_metrics.pending.set(0);
                                log::warn!("OTLP shutdown deadline: discarded {remaining} access records");
                            }
                        }
                    }
                });
                // DNS resolution may use Tokio's blocking pool; it must not extend process shutdown.
                runtime.shutdown_timeout(Duration::from_millis(100));
            })
            .context("cannot start OTLP access log worker")?;
        Ok(Some(Self {
            sender: ArcSwapOption::from(Some(Arc::new(sender))),
            metrics,
            worker: Mutex::new(Some((stop, worker))),
        }))
    }

    pub fn access(&self, record: AccessRecord<'_>) {
        self.record(Record::Log(record.into_log()));
    }
    pub fn span(&self, span: Span) {
        self.record(Record::Span(span));
    }
    fn record(&self, record: Record) {
        if record.encoded_len() > MAX_RECORD_BYTES {
            self.metrics.drop_records(1, "record_too_large");
            return;
        }
        if let Some(sender) = self.sender.load().as_ref() {
            self.metrics.pending.inc();
            if sender.try_send(record).is_err() {
                self.metrics.pending.dec();
                self.metrics.drop_records(1, "queue_full");
            }
        } else {
            self.metrics.drop_records(1, "shutdown");
        }
    }

    /// Called after Pingora finishes draining requests, with a five-second export deadline.
    pub fn shutdown(&self) {
        self.sender.store(None);
        if let Some((stop, worker)) = self.worker.lock().unwrap().take() {
            let _ = stop.send(());
            if worker.join().is_err() {
                log::error!("OTLP access log worker panicked");
            }
        }
    }
}
impl Drop for Exporter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn run(config: config::Config, mut receiver: mpsc::Receiver<Record>, metrics: &Metrics) {
    while let Some(first) = receiver.recv().await {
        let mut size = first.encoded_len();
        let mut records = vec![first];
        let deadline = tokio::time::Instant::now() + config.interval;
        // Leave space for a maximum-sized record, Resource and Scope protobuf envelopes.
        while records.len() < config.batch_size && size < MAX_BATCH_BYTES - MAX_RECORD_BYTES - 65536
        {
            match tokio::time::timeout_at(deadline, receiver.recv()).await {
                Ok(Some(record)) => {
                    size += record.encoded_len();
                    records.push(record);
                }
                _ => break,
            }
        }
        let count = records.len();
        let scope = Some(InstrumentationScope {
            name: match config.signal {
                config::Signal::Logs => "rgnix.access",
                config::Signal::Traces => "rgnix.proxy",
            }
            .into(),
            version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        });
        let payload = Bytes::from(match config.signal {
            config::Signal::Logs => ExportLogsServiceRequest {
                resource_logs: vec![ResourceLogs {
                    resource: Some(config.resource.clone()),
                    scope_logs: vec![ScopeLogs {
                        scope,
                        log_records: records
                            .into_iter()
                            .filter_map(|r| {
                                if let Record::Log(r) = r {
                                    Some(r)
                                } else {
                                    None
                                }
                            })
                            .collect(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            }
            .encode_to_vec(),
            config::Signal::Traces => ExportTraceServiceRequest {
                resource_spans: vec![ResourceSpans {
                    resource: Some(config.resource.clone()),
                    scope_spans: vec![ScopeSpans {
                        scope,
                        spans: records
                            .into_iter()
                            .filter_map(|r| {
                                if let Record::Span(r) = r {
                                    Some(r)
                                } else {
                                    None
                                }
                            })
                            .collect(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            }
            .encode_to_vec(),
        });
        let result =
            tokio::time::timeout(config.timeout, export(&config, payload, count, metrics)).await;
        match result {
            Ok(Some(rejected)) => {
                metrics.exported.inc_by((count - rejected) as u64);
                if rejected != 0 {
                    metrics.drop_records(rejected, "remote_rejected");
                }
            }
            _ => {
                if result.is_err() {
                    metrics.errors.inc();
                }
                metrics.drop_records(count, "export_failed");
            }
        }
        metrics.pending.sub(count as i64);
    }
}

enum Attempt {
    Accepted { rejected: usize, partial: bool },
    Retry(Option<Duration>),
    Failed,
}

async fn export(
    config: &config::Config,
    payload: Bytes,
    count: usize,
    metrics: &Metrics,
) -> Option<usize> {
    for attempt in 0..3 {
        let result = send(config, payload.clone(), count).await;
        match result {
            Attempt::Accepted { rejected, partial } => {
                if partial {
                    metrics.partial.inc();
                }
                return Some(rejected);
            }
            Attempt::Retry(delay) if attempt < 2 => {
                metrics.errors.inc();
                let delay = delay.unwrap_or_else(|| {
                    Duration::from_millis((200 << attempt) + u64::from(rand::random::<u8>()))
                });
                tokio::time::sleep(delay).await;
                metrics.retries.inc();
            }
            _ => {
                metrics.errors.inc();
                return None;
            }
        }
    }
    None
}

async fn send(config: &config::Config, payload: Bytes, count: usize) -> Attempt {
    let response = config
        .client
        .post(config.endpoint.clone())
        .header("Content-Type", "application/x-protobuf")
        .header("Accept", "application/x-protobuf")
        .body(payload)
        .send()
        .await;
    let mut response = match response {
        Ok(response) => response,
        // Never log client errors: they may contain endpoint query credentials.
        Err(_) => return Attempt::Retry(None),
    };
    if matches!(response.status().as_u16(), 429 | 502 | 503 | 504) {
        let delay = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                    httpdate::parse_http_date(v)
                        .ok()
                        .map(|t| t.duration_since(SystemTime::now()).unwrap_or_default())
                })
            });
        return Attempt::Retry(delay);
    }
    if response.status() != reqwest::StatusCode::OK
        || response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_none_or(|v| v.split(';').next().unwrap_or("").trim() != "application/x-protobuf")
        || response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Attempt::Failed;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= MAX_RESPONSE_BYTES => {
                body.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            Ok(Some(_)) => return Attempt::Failed,
            Err(_) => return Attempt::Retry(None),
        }
    }
    // OTLP logs and traces share the partial-success wire shape: field 1 is
    // rejected records/spans, field 2 is an optional error message.
    match ExportLogsServiceResponse::decode(body.as_slice()) {
        Ok(response) => {
            let partial = response.partial_success.is_some();
            let rejected = response
                .partial_success
                .map_or(0, |p| p.rejected_log_records);
            if rejected < 0 || rejected as u64 > count as u64 {
                return Attempt::Failed;
            }
            Attempt::Accepted {
                rejected: rejected as usize,
                partial,
            }
        }
        Err(_) => Attempt::Failed,
    }
}
