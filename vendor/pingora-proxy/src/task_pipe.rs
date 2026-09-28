use crate::TASK_BUFFER_SIZE;
use pingora_core::protocols::http::HttpTask;
use std::future::poll_fn;
use std::sync::{Mutex, MutexGuard};
use std::task::{ready, Poll, Waker};
use tokio::sync::mpsc::error::{SendError, TrySendError};

// The two halves of an HTTP/1 exchange are joined in one future. Borrowing its
// bounded queues avoids a heap-owned, multi-producer channel per direction.
// The mutex keeps the future Send without relying on worker affinity.
pub(super) struct TaskPipe(Mutex<State>);

struct State {
    tasks: [Option<HttpTask>; TASK_BUFFER_SIZE],
    head: usize,
    len: usize,
    reserved: usize,
    sender_open: bool,
    receiver_open: bool,
    reader: Option<Waker>,
    writer: Option<Waker>,
}

impl TaskPipe {
    pub fn new() -> Self {
        Self(Mutex::new(State {
            tasks: std::array::from_fn(|_| None),
            head: 0,
            len: 0,
            reserved: 0,
            sender_open: true,
            receiver_open: true,
            reader: None,
            writer: None,
        }))
    }

    pub fn split(&mut self) -> (Sender<'_>, Receiver<'_>) {
        (Sender(self), Receiver(self))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
}

pub(super) struct Sender<'a>(&'a TaskPipe);
pub(super) struct Receiver<'a>(&'a TaskPipe);
pub(super) struct Permit<'a>(Option<&'a TaskPipe>);

impl std::fmt::Debug for Permit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TaskPipePermit")
    }
}

impl Sender<'_> {
    pub fn is_closed(&self) -> bool {
        !self.0.lock().receiver_open
    }

    pub fn try_reserve(&self) -> Result<Permit<'_>, TrySendError<()>> {
        let mut state = self.0.lock();
        if !state.receiver_open {
            return Err(TrySendError::Closed(()));
        }
        if state.len + state.reserved == TASK_BUFFER_SIZE {
            return Err(TrySendError::Full(()));
        }
        state.reserved += 1;
        Ok(Permit(Some(self.0)))
    }

    pub async fn reserve(&self) -> Result<Permit<'_>, SendError<()>> {
        poll_fn(|cx| {
            let budget = ready!(tokio::task::coop::poll_proceed(cx));
            let mut state = self.0.lock();
            if !state.receiver_open {
                budget.made_progress();
                return Poll::Ready(Err(SendError(())));
            }
            if state.len + state.reserved < TASK_BUFFER_SIZE {
                state.reserved += 1;
                budget.made_progress();
                return Poll::Ready(Ok(Permit(Some(self.0))));
            }
            match &mut state.writer {
                Some(waker) => waker.clone_from(cx.waker()),
                slot => *slot = Some(cx.waker().clone()),
            }
            Poll::Pending
        })
        .await
    }

    pub async fn send(&self, task: HttpTask) -> Result<(), SendError<HttpTask>> {
        match self.reserve().await {
            Ok(permit) => {
                permit.send(task);
                Ok(())
            }
            Err(_) => Err(SendError(task)),
        }
    }
}

impl Permit<'_> {
    pub fn send(mut self, task: HttpTask) {
        let pipe = self.0.take().unwrap();
        let mut task = Some(task);
        let wake = {
            let mut state = pipe.lock();
            state.reserved -= 1;
            if state.receiver_open {
                let tail = (state.head + state.len) % TASK_BUFFER_SIZE;
                state.tasks[tail] = task.take();
                state.len += 1;
            }
            state.reader.take()
        };
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Some(pipe) = self.0.take() {
            let wake = {
                let mut state = pipe.lock();
                state.reserved -= 1;
                state.writer.take()
            };
            if let Some(waker) = wake {
                waker.wake();
            }
        }
    }
}

impl Receiver<'_> {
    pub async fn recv(&mut self) -> Option<HttpTask> {
        poll_fn(|cx| {
            let budget = ready!(tokio::task::coop::poll_proceed(cx));
            let mut state = self.0.lock();
            if state.len != 0 {
                let head = state.head;
                let task = state.tasks[head].take();
                state.head = (head + 1) % TASK_BUFFER_SIZE;
                state.len -= 1;
                let wake = state.writer.take();
                drop(state);
                if let Some(waker) = wake {
                    waker.wake();
                }
                budget.made_progress();
                return Poll::Ready(task);
            }
            if !state.sender_open {
                budget.made_progress();
                return Poll::Ready(None);
            }
            match &mut state.reader {
                Some(waker) => waker.clone_from(cx.waker()),
                slot => *slot = Some(cx.waker().clone()),
            }
            Poll::Pending
        })
        .await
    }
}

impl Drop for Sender<'_> {
    fn drop(&mut self) {
        let wake = {
            let mut state = self.0.lock();
            state.sender_open = false;
            state.reader.take()
        };
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}

impl Drop for Receiver<'_> {
    fn drop(&mut self) {
        let (wake, tasks) = {
            let mut state = self.0.lock();
            state.receiver_open = false;
            state.len = 0;
            (
                state.writer.take(),
                std::mem::replace(&mut state.tasks, std::array::from_fn(|_| None)),
            )
        };
        drop(tasks);
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::{pin_mut, poll};

    #[tokio::test(flavor = "current_thread")]
    async fn ready_exchange_yields_to_other_tasks() {
        let progress = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observer = tokio::spawn({
            let progress = progress.clone();
            async move {
                progress.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let mut pipe = TaskPipe::new();
        let (tx, mut rx) = pipe.split();
        for _ in 0..10_000 {
            tx.send(HttpTask::Body(None, false)).await.unwrap();
            assert!(rx.recv().await.is_some());
            if progress.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
        }
        assert!(
            progress.load(std::sync::atomic::Ordering::Relaxed),
            "a continuously ready exchange must not starve unrelated work"
        );
        observer.await.unwrap();
    }

    #[tokio::test]
    async fn bounded_exchange_drains_in_order_and_closes() {
        let mut pipe = TaskPipe::new();
        let (tx, mut rx) = pipe.split();
        let send = async {
            for value in 0..100_u8 {
                tx.send(HttpTask::Body(Some(Bytes::from(vec![value])), false))
                    .await
                    .unwrap();
            }
            drop(tx);
        };
        let receive = async {
            for value in 0..100_u8 {
                let Some(HttpTask::Body(Some(body), false)) = rx.recv().await else {
                    panic!("body lost or pipe closed before all messages arrived");
                };
                assert_eq!(&body[..], &[value]);
            }
            assert!(rx.recv().await.is_none());
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(send, receive);
        })
        .await
        .expect("bounded producer and consumer must wake each other");
    }

    #[tokio::test]
    async fn cancelled_reservation_releases_capacity_and_receiver_close_wakes_sender() {
        let mut pipe = TaskPipe::new();
        let (tx, rx) = pipe.split();
        let mut permits: Vec<_> = (0..TASK_BUFFER_SIZE)
            .map(|_| tx.try_reserve().unwrap())
            .collect();
        assert!(matches!(tx.try_reserve(), Err(TrySendError::Full(()))));
        let pending = tx.reserve();
        pin_mut!(pending);
        assert!(poll!(&mut pending).is_pending());
        drop(permits.pop());
        let extra = pending.await.unwrap();
        drop(extra);
        drop(permits);
        for _ in 0..TASK_BUFFER_SIZE {
            tx.send(HttpTask::Body(None, true)).await.unwrap();
        }
        let pending = tx.send(HttpTask::Body(None, true));
        pin_mut!(pending);
        assert!(poll!(&mut pending).is_pending());
        drop(rx);
        assert!(pending.await.is_err());
        assert!(tx.is_closed());
    }
}
