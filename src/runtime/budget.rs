use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// A process-wide request limit with immediate rejection and no waiting queue.
pub struct RequestBudget {
    available: AtomicUsize,
}

/// Holds one request slot until completion or cancellation.
pub struct RequestPermit(Arc<RequestBudget>);

impl RequestBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            available: AtomicUsize::new(limit),
        }
    }

    pub fn available_permits(&self) -> usize {
        self.available.load(Ordering::Acquire)
    }

    pub fn try_acquire_owned(
        self: Arc<Self>,
    ) -> Result<RequestPermit, tokio::sync::TryAcquireError> {
        self.available
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |n| n.checked_sub(1))
            .map_err(|_| tokio::sync::TryAcquireError::NoPermits)?;
        Ok(RequestPermit(self))
    }
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        self.0.available.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_requests_preserve_the_global_limit_and_restore_slots() {
        let budget = Arc::new(RequestBudget::new(4));
        let active = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let budget = budget.clone();
                let active = &active;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        let permit = loop {
                            if let Ok(permit) = budget.clone().try_acquire_owned() {
                                break permit;
                            }
                            std::thread::yield_now();
                        };
                        assert!(active.fetch_add(1, Ordering::SeqCst) < 4);
                        std::thread::yield_now();
                        active.fetch_sub(1, Ordering::SeqCst);
                        drop(permit);
                    }
                });
            }
        });
        assert_eq!(active.load(Ordering::Relaxed), 0);
        assert_eq!(budget.available_permits(), 4);
        let permits: Vec<_> = (0..4)
            .map(|_| budget.clone().try_acquire_owned().ok().unwrap())
            .collect();
        assert!(budget.clone().try_acquire_owned().is_err());
        drop(permits);
        assert_eq!(budget.available_permits(), 4);
    }
}
