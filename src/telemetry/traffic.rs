use anyhow::Result;
use prometheus::{HistogramVec, IntCounter, IntCounterVec, Registry};

pub struct Traffic {
    #[cfg(feature = "hyper-experimental")]
    pub hyper_connections: prometheus::IntGauge,
    #[cfg(feature = "hyper-experimental")]
    pub hyper_pending: prometheus::IntGauge,
    #[cfg(feature = "hyper-experimental")]
    pub hyper_admission_rejected: IntCounterVec,
    #[cfg(feature = "hyper-experimental")]
    pub hyper_accept_errors: IntCounterVec,
    #[cfg(feature = "hyper-experimental")]
    pub hyper_rejected: IntCounter,
    #[cfg(feature = "hyper-experimental")]
    pub hyper_handshake_errors: IntCounter,
    pub request_bytes: IntCounter,
    pub response_bytes: IntCounter,
    pub failures: IntCounterVec,
    pub upstream_connect: HistogramVec,
    pub upstream_headers: HistogramVec,
    pub upstream_duration: HistogramVec,
    pub inspection: HistogramVec,
    pub inspection_bytes: IntCounterVec,
    pub shared_limit: IntCounterVec,
    pub shared_limit_seconds: HistogramVec,
}

impl Traffic {
    pub fn new(registry: &Registry) -> Result<Self> {
        use prometheus::*;
        Ok(Self {
            #[cfg(feature = "hyper-experimental")]
            hyper_connections: register_int_gauge_with_registry!(
                "rgnix_hyper_connections",
                "Accepted Hyper connections including handshakes and tunnels",
                registry
            )?,
            #[cfg(feature = "hyper-experimental")]
            hyper_pending: register_int_gauge_with_registry!(
                "rgnix_hyper_pending_connections",
                "Connections waiting for handshake or initial request headers",
                registry
            )?,
            #[cfg(feature = "hyper-experimental")]
            hyper_admission_rejected: register_int_counter_vec_with_registry!(
                "rgnix_hyper_admission_rejections_total",
                "Rejected connection admission by bounded policy reason",
                &["reason"],
                registry
            )?,
            #[cfg(feature = "hyper-experimental")]
            hyper_accept_errors: register_int_counter_vec_with_registry!(
                "rgnix_hyper_accept_errors_total",
                "Listener accept failures by recovery class",
                &["reason"],
                registry
            )?,
            #[cfg(feature = "hyper-experimental")]
            hyper_rejected: register_int_counter_with_registry!(
                "rgnix_hyper_connection_rejections_total",
                "Hyper connections rejected by the process connection budget",
                registry
            )?,
            #[cfg(feature = "hyper-experimental")]
            hyper_handshake_errors: register_int_counter_with_registry!(
                "rgnix_hyper_tls_handshake_errors_total",
                "Failed downstream TLS handshakes",
                registry
            )?,
            shared_limit: register_int_counter_vec_with_registry!(
                "rgnix_global_rate_limit_total",
                "Shared admission decisions and dependency fallbacks",
                &["result"],
                registry
            )?,
            shared_limit_seconds: register_histogram_vec_with_registry!(
                "rgnix_global_rate_limit_seconds",
                "Shared admission time including coordinator access",
                &["result"],
                registry
            )?,
            request_bytes: register_int_counter_with_registry!(
                "rgnix_request_body_bytes_total",
                "Downstream body bytes read, accounted when a request finishes",
                registry
            )?,
            response_bytes: register_int_counter_with_registry!(
                "rgnix_response_body_bytes_total",
                "Downstream body bytes sent, accounted when a request finishes",
                registry
            )?,
            failures: register_int_counter_vec_with_registry!(
                "rgnix_request_failures_total",
                "Requests ending with a proxy error, including failures after response headers",
                &["source", "reason"],
                registry
            )?,
            upstream_connect: register_histogram_vec_with_registry!(
                "rgnix_upstream_connect_seconds",
                "Successful upstream connection acquisition, including TLS or pool reuse",
                &["backend", "reused"],
                registry
            )?,
            upstream_headers: register_histogram_vec_with_registry!(
                "rgnix_upstream_header_seconds",
                "Time from upstream selection to response headers, including request upload",
                &["backend"],
                registry
            )?,
            upstream_duration: register_histogram_vec_with_registry!(
                "rgnix_upstream_request_seconds",
                "Time from upstream selection to request completion",
                &["backend"],
                registry
            )?,
            inspection: register_histogram_vec_with_registry!(
                "rgnix_body_inspection_seconds",
                "Request body inspection duration and outcome",
                &["mode", "result"],
                registry
            )?,
            inspection_bytes: register_int_counter_vec_with_registry!(
                "rgnix_body_inspected_bytes_total",
                "Bytes exposed to successful request body inspection",
                &["mode"],
                registry
            )?,
        })
    }

    pub fn failure(&self, error: &pingora::Error) {
        use pingora::ErrorSource;
        let source = match error.esource() {
            ErrorSource::Upstream => "upstream",
            ErrorSource::Downstream => "downstream",
            _ => "internal",
        };
        self.failures
            .with_label_values(&[source, error_reason(error)])
            .inc();
    }
}

pub(crate) fn error_reason(error: &pingora::Error) -> &'static str {
    use pingora::ErrorType;
    match error.etype() {
        ErrorType::ConnectTimedout => "connect_timeout",
        ErrorType::TLSHandshakeTimedout => "tls_timeout",
        ErrorType::ReadTimedout => "read_timeout",
        ErrorType::Custom("UpstreamTimedout") => "upstream_timeout",
        ErrorType::WriteTimedout => "write_timeout",
        ErrorType::ReadError => "read",
        ErrorType::WriteError => "write",
        ErrorType::ConnectionClosed => "closed",
        ErrorType::HTTPStatus(408) => "request_timeout",
        ErrorType::HTTPStatus(413) => "body_limit",
        ErrorType::HTTPStatus(_) => "http_status",
        _ => "other",
    }
}
