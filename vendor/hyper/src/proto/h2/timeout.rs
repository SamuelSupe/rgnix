use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::{Instant, Sleep};

pub(crate) struct HeaderTimeout {
    progress: Arc<Mutex<(Instant, bool)>>,
    duration: Duration,
    timer: Pin<Box<Sleep>>,
}
pub(crate) struct BodyTimeout {
    progress: Option<Arc<Mutex<(Instant, bool)>>>,
    duration: Duration,
    timer: Option<Pin<Box<Sleep>>>,
}
impl HeaderTimeout {
    pub(crate) fn pair(policy: crate::ext::H2Timeouts) -> (Self, BodyTimeout) {
        let now = Instant::now();
        let progress = Arc::new(Mutex::new((now, false)));
        (
            Self {
                progress: progress.clone(),
                duration: policy.read,
                timer: Box::pin(tokio::time::sleep_until(now + policy.read)),
            },
            BodyTimeout {
                progress: Some(progress),
                duration: policy.write,
                timer: None,
            },
        )
    }
    pub(crate) fn failed(&self) -> bool {
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).1
    }
    pub(crate) fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        if self.timer.as_mut().poll(cx).is_pending() {
            return false;
        }
        let deadline = self.progress.lock().unwrap_or_else(|e| e.into_inner()).0 + self.duration;
        if deadline <= Instant::now() {
            return true;
        }
        self.timer.as_mut().reset(deadline);
        self.timer.as_mut().poll(cx).is_ready()
    }
}
impl BodyTimeout {
    pub(crate) fn new(duration: Duration) -> Self {
        Self {
            progress: None,
            duration,
            timer: None,
        }
    }
    pub(crate) fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        let expired = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.duration)))
            .as_mut()
            .poll(cx)
            == Poll::Ready(());
        if expired {
            if let Some(progress) = &self.progress {
                progress.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
            }
        }
        expired
    }
    pub(crate) fn progress(&mut self) {
        self.timer = None;
        if let Some(progress) = &self.progress {
            progress.lock().unwrap_or_else(|e| e.into_inner()).0 = Instant::now();
        }
    }
}
