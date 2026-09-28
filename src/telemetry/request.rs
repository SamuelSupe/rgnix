use super::Telemetry;
use prometheus::{Histogram, IntCounter};
use std::sync::OnceLock;

#[derive(Clone)]
pub(crate) struct RouteMetrics {
    label: String,
    overflow: bool,
    requests: [OnceLock<IntCounter>; 10],
    grpc: [OnceLock<IntCounter>; 17],
    duration: Histogram,
}

impl RouteMetrics {
    pub(super) fn new(telemetry: &Telemetry, route: &str) -> Self {
        let label = telemetry.resolve_label("route", route);
        Self {
            label: label.into(),
            overflow: label != route,
            requests: Default::default(),
            grpc: Default::default(),
            duration: telemetry.route_duration.with_label_values(&[label]),
        }
    }

    pub(super) fn completed(
        &self,
        telemetry: &Telemetry,
        status: u16,
        seconds: f64,
        grpc: Option<u16>,
    ) {
        if self.overflow {
            telemetry.label_overflow.with_label_values(&["route"]).inc();
        }
        let class = usize::from(status / 100);
        let count = || {
            telemetry
                .route_requests
                .with_label_values(&[&self.label, &format!("{class}xx")])
        };
        if let Some(slot) = self.requests.get(class) {
            slot.get_or_init(count).inc();
        } else {
            count().inc();
        }
        self.duration.observe(seconds);
        if let Some(code) = grpc {
            let count = || {
                telemetry
                    .grpc_requests
                    .with_label_values(&[&self.label, &code.to_string()])
            };
            if let Some(slot) = self.grpc.get(usize::from(code)) {
                slot.get_or_init(count).inc();
            } else {
                count().inc();
            }
        }
    }
}

impl Telemetry {
    pub(crate) fn request_completed(&self, status: u16) {
        let count = || self.requests.with_label_values(&[&status.to_string()]);
        if let Some(slot) = self.status_counts.get(usize::from(status)) {
            slot.get_or_init(count).inc();
        } else {
            count().inc();
        }
    }
}
