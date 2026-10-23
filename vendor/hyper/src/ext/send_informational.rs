use atomic_waker::AtomicWaker;
use http::Response;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
};

/// Per-request sender installed in server request extensions. Only 102 and 103
/// are accepted; 100 Continue belongs to the decoder and 101 to upgrade handling.
/// A full queue, closed request, or headers over 64 KiB rejects the response.
#[derive(Debug)]
pub struct SendInformational {
    state: Arc<State>,
    request: Arc<RequestState>,
}

#[derive(Debug)]
struct State {
    responses: Mutex<VecDeque<Response<()>>>,
    active_request: AtomicUsize,
    queued: AtomicBool,
    waker: AtomicWaker,
}

#[derive(Debug)]
struct RequestState {
    senders: AtomicUsize,
}

#[derive(Debug)]
/// A bounded stream of interim responses. Dropping it closes all associated senders.
pub struct Receiver {
    state: Arc<State>,
    request: Arc<RequestState>,
}

fn request_id(request: &Arc<RequestState>) -> usize {
    Arc::as_ptr(request) as usize
}

impl Clone for SendInformational {
    fn clone(&self) -> Self {
        self.request.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            state: self.state.clone(),
            request: self.request.clone(),
        }
    }
}

impl SendInformational {
    /// Create a queue for a transport adapter. Drain it before sending the final
    /// response; it closes when the last sender is dropped or the receiver is dropped.
    pub fn channel() -> (Self, Receiver) {
        channel()
    }
    /// Queue an interim response without completing the request or running final hooks.
    pub fn try_send(&self, mut response: Response<()>) -> Result<(), Response<()>> {
        if !matches!(response.status().as_u16(), 102 | 103)
            || response
                .headers()
                .iter()
                .map(|(k, v)| k.as_str().len() + v.len() + 4)
                .sum::<usize>()
                > 65536
        {
            return Err(response);
        }
        for name in [
            "connection",
            "content-length",
            "transfer-encoding",
            "upgrade",
            "trailer",
        ] {
            response.headers_mut().remove(name);
        }
        let mut responses = self
            .state
            .responses
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.state.active_request.load(Ordering::Acquire) != request_id(&self.request)
            || responses.len() == 4
        {
            return Err(response);
        }
        responses.push_back(response);
        self.state.queued.store(true, Ordering::Release);
        drop(responses);
        self.state.waker.wake();
        Ok(())
    }
}

impl Drop for SendInformational {
    fn drop(&mut self) {
        if self.request.senders.fetch_sub(1, Ordering::AcqRel) == 1
            && self.state.active_request.load(Ordering::Acquire) == request_id(&self.request)
        {
            self.state.waker.wake();
        }
    }
}

impl Receiver {
    pub(crate) fn begin_request(&mut self) -> SendInformational {
        if let Some(request) = Arc::get_mut(&mut self.request) {
            *request.senders.get_mut() = 1;
        } else {
            self.request = Arc::new(RequestState {
                senders: AtomicUsize::new(1),
            });
        }
        let mut responses = self
            .state
            .responses
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        responses.clear();
        self.state.queued.store(false, Ordering::Release);
        // Only a uniquely owned token may be reused. Old senders retain their
        // token, so its address cannot be reused while a stale sender exists.
        // Activation under the queue lock excludes racing old hints.
        self.state
            .active_request
            .store(request_id(&self.request), Ordering::Release);
        drop(responses);
        drop(self.state.waker.take());
        SendInformational {
            state: self.state.clone(),
            request: self.request.clone(),
        }
    }

    pub(crate) fn end_request(&mut self) {
        self.state.active_request.store(0, Ordering::Release);
        drop(self.state.waker.take());
    }

    /// Poll the next interim response, or `None` after the last sender closes.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Response<()>>> {
        if self.state.active_request.load(Ordering::Acquire) == 0 {
            return Poll::Ready(None);
        }
        if !self.state.queued.load(Ordering::Acquire) {
            if self.request.senders.load(Ordering::Acquire) == 0
                && !self.state.queued.load(Ordering::Acquire)
            {
                return Poll::Ready(None);
            }
            self.state.waker.register(cx.waker());
            // Read closure before rechecking the queue: the last sender may
            // have queued a hint immediately before dropping. Empty polls need
            // no lock, but neither queued hints nor wakeups may be lost.
            let closed = self.request.senders.load(Ordering::Acquire) == 0;
            if !self.state.queued.load(Ordering::Acquire) {
                return if closed {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                };
            }
        }
        let mut responses = self
            .state
            .responses
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let response = responses.pop_front();
        self.state
            .queued
            .store(!responses.is_empty(), Ordering::Release);
        Poll::Ready(response)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.end_request();
        self.state
            .responses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

pub(crate) fn channel() -> (SendInformational, Receiver) {
    // Tokio mpsc reserves a block of Response slots even when no hints are sent.
    // Most requests send none, so allocate queue storage only for actual hints.
    let request = Arc::new(RequestState {
        senders: AtomicUsize::new(1),
    });
    let state = Arc::new(State {
        responses: Mutex::new(VecDeque::new()),
        active_request: AtomicUsize::new(request_id(&request)),
        queued: AtomicBool::new(false),
        waker: AtomicWaker::new(),
    });
    (
        SendInformational {
            state: state.clone(),
            request: request.clone(),
        },
        Receiver { state, request },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::poll_fn, time::Duration};

    #[tokio::test]
    async fn bounded_hints_and_closed_senders_wake_receiver() {
        let (sender, mut receiver) = channel();
        let hint = || Response::builder().status(103).body(()).unwrap();
        for _ in 0..4 {
            sender.try_send(hint()).unwrap();
        }
        assert!(sender.try_send(hint()).is_err());
        assert_eq!(
            poll_fn(|cx| receiver.poll_recv(cx)).await.unwrap().status(),
            103
        );
        sender.try_send(hint()).unwrap();
        receiver.end_request();
        assert!(sender.try_send(hint()).is_err());
        let current = receiver.begin_request();
        assert!(sender.try_send(hint()).is_err());
        drop(sender);
        current.try_send(hint()).unwrap();
        assert_eq!(
            poll_fn(|cx| receiver.poll_recv(cx)).await.unwrap().status(),
            103
        );
        drop(current);
        receiver.end_request();
        let current = receiver.begin_request();
        current
            .try_send(Response::builder().status(102).body(()).unwrap())
            .unwrap();
        assert_eq!(
            poll_fn(|cx| receiver.poll_recv(cx)).await.unwrap().status(),
            102
        );
        drop(receiver);
        assert!(current.try_send(hint()).is_err());

        let (sender, mut receiver) = channel();
        let another = sender.clone();
        let waiting = tokio::spawn(async move { poll_fn(|cx| receiver.poll_recv(cx)).await });
        tokio::task::yield_now().await;
        drop(sender);
        assert!(!waiting.is_finished());
        drop(another);
        assert!(tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap()
            .is_none());

        let (sender, mut receiver) = channel();
        let waiting = tokio::spawn(async move { poll_fn(|cx| receiver.poll_recv(cx)).await });
        tokio::task::yield_now().await;
        sender.try_send(hint()).unwrap();
        drop(sender);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .status(),
            103
        );
    }
}
