use crate::telemetry::Telemetry;
use async_trait::async_trait;
use pingora::{server::ShutdownWatch, services::background::BackgroundService};
use std::{path::PathBuf, sync::Arc, time::Duration};

pub(super) struct Drain {
    pub marker: Option<PathBuf>,
    pub telemetry: Arc<Telemetry>,
}
#[async_trait]
impl BackgroundService for Drain {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = shutdown.changed() => { self.telemetry.draining.set(1); break; }
                _ = tick.tick() => {
                    if self.marker.as_ref().is_some_and(|path| path.exists()) {
                        self.telemetry.draining.set(1);
                    }
                }
            }
        }
    }
}
