use super::{test_reusable_stream, PoolCallback};
use crate::protocols::Stream;
use lru::LruCache;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Idle {
    stream: Stream,
    since: Instant,
    deadline: Option<Instant>,
}

struct State {
    groups: LruCache<u64, VecDeque<Idle>>,
    len: usize,
}

pub(super) struct TransportPool {
    state: Mutex<State>,
    capacity: usize,
    started: AtomicBool,
    unexpected: Arc<AtomicU64>,
    callback: Option<PoolCallback>,
}

impl TransportPool {
    pub fn new(
        capacity: usize,
        unexpected: Arc<AtomicU64>,
        callback: Option<PoolCallback>,
    ) -> Self {
        Self {
            state: Mutex::new(State {
                groups: LruCache::new(NonZeroUsize::new(capacity.max(1)).unwrap()),
                len: 0,
            }),
            capacity,
            started: AtomicBool::new(false),
            unexpected,
            callback,
        }
    }

    pub fn get(&self, key: u64) -> Option<Stream> {
        loop {
            let idle = {
                let mut state = self.state.lock();
                let idle = state.groups.get_mut(&key)?.pop_back()?;
                state.len -= 1;
                idle
            };
            let now = Instant::now();
            if idle.deadline.is_none_or(|deadline| now < deadline) {
                return Some(idle.stream);
            }
            self.discard(idle, now);
        }
    }

    pub fn put(self: &Arc<Self>, key: u64, stream: Stream, timeout: Option<Duration>) {
        if self.capacity == 0 || timeout == Some(Duration::ZERO) {
            return;
        }
        let now = Instant::now();
        let idle = Idle {
            stream,
            since: now,
            deadline: timeout.and_then(|timeout| now.checked_add(timeout)),
        };
        let mut discarded = None;
        let mut displaced_group = None;
        {
            let mut state = self.state.lock();
            if state.len == self.capacity {
                // Evict the oldest connection in the least recently used peer
                // group. Empty groups are retained, bounded by the pool capacity,
                // so a busy peer does not allocate a new index on every request.
                while let Some((&oldest, _)) = state.groups.peek_lru() {
                    if let Some(entry) = state.groups.peek_mut(&oldest).unwrap().pop_front() {
                        state.len -= 1;
                        discarded = Some(entry);
                        break;
                    }
                    state.groups.pop_lru();
                }
            }
            if !state.groups.contains(&key) {
                if state.groups.len() == self.capacity {
                    // There are fewer idle streams than index slots here, so
                    // an empty group exists. Reclaim it rather than evicting
                    // every live connection of an older, busy peer.
                    let empty = state
                        .groups
                        .iter()
                        .rev()
                        .find_map(|(&key, entries)| entries.is_empty().then_some(key))
                        .unwrap();
                    displaced_group = state.groups.pop(&empty);
                }
                state.groups.put(key, VecDeque::new());
            }
            state.groups.get_mut(&key).unwrap().push_back(idle);
            state.len += 1;
        }
        if let Some(idle) = discarded {
            self.discard(idle, now);
        }
        drop(displaced_group);
        if !self.started.swap(true, Ordering::Relaxed) {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let Some(pool) = weak.upgrade() else { break };
                    pool.sweep();
                }
            });
        }
    }

    fn discard(&self, idle: Idle, now: Instant) {
        let age = now.saturating_duration_since(idle.since);
        drop(idle);
        if let Some(callback) = &self.callback {
            callback(age);
        }
    }

    fn sweep(&self) {
        let now = Instant::now();
        let mut discarded = Vec::new();
        {
            let mut state = self.state.lock();
            for (_, entries) in state.groups.iter_mut() {
                let mut index = 0;
                while index < entries.len() {
                    let idle = &mut entries[index];
                    if idle.deadline.is_some_and(|deadline| now >= deadline)
                        || !test_reusable_stream(&mut idle.stream, &self.unexpected)
                    {
                        discarded.push(entries.remove(index).unwrap());
                    } else {
                        index += 1;
                    }
                }
                if entries.capacity() > entries.len().saturating_mul(4).max(16) {
                    entries.shrink_to(entries.len().saturating_mul(2).max(16));
                }
            }
            state.len -= discarded.len();
        }
        for idle in discarded {
            self.discard(idle, now);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::protocols::l4::stream::Stream as SocketStream;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    fn connection() -> (Stream, UnixStream) {
        let (client, server) = UnixStream::pair().unwrap();
        (Box::new(SocketStream::from(client)), server)
    }

    #[tokio::test]
    async fn bounded_pool_expires_and_rejects_unsolicited_data() {
        let unexpected = Arc::new(AtomicU64::new(0));
        let pool = Arc::new(TransportPool::new(2, unexpected.clone(), None));
        let (first, mut first_peer) = connection();
        let (second, mut second_peer) = connection();
        let (third, mut third_peer) = connection();
        pool.put(1, first, None);
        pool.put(2, second, None);
        pool.put(3, third, Some(Duration::from_millis(20)));
        let mut buf = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), first_peer.read(&mut buf))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        second_peer.write_all(b"unexpected").await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(pool.get(3).is_none());
        assert_eq!(third_peer.read(&mut buf).await.unwrap(), 0);
        pool.sweep();
        assert!(pool.get(2).is_none());
        assert_eq!(unexpected.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn checkout_transfers_exclusive_ownership_and_pool_drop_closes_idle_streams() {
        let pool = Arc::new(TransportPool::new(1, Arc::new(AtomicU64::new(0)), None));
        let (stream, mut peer) = connection();
        pool.put(9, stream, None);
        let mut stream = pool.get(9).unwrap();
        assert!(pool.get(9).is_none());
        stream.write_all(b"ok").await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = [0; 2];
        peer.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ok");
        pool.put(9, stream, None);
        drop(pool);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), peer.read(&mut buf))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn empty_peer_indexes_do_not_displace_live_idle_connections() {
        let pool = Arc::new(TransportPool::new(3, Arc::new(AtomicU64::new(0)), None));
        let (first, _first_peer) = connection();
        let (second, _second_peer) = connection();
        pool.put(1, first, None);
        pool.put(1, second, None);
        for key in [2, 3] {
            let (stream, _peer) = connection();
            pool.put(key, stream, None);
            assert!(pool.get(key).is_some());
        }
        let (stream, _peer) = connection();
        pool.put(4, stream, None);
        assert!(pool.get(1).is_some());
        assert!(pool.get(1).is_some());
        assert!(pool.get(4).is_some());
    }

    #[tokio::test]
    async fn checkout_checks_expiry_after_waiting_for_the_pool_lock() {
        let pool = Arc::new(TransportPool::new(1, Arc::new(AtomicU64::new(0)), None));
        let (stream, _peer) = connection();
        pool.put(1, stream, Some(Duration::from_millis(100)));
        let state = pool.state.lock();
        let waiting = pool.clone();
        let (started, received) = std::sync::mpsc::channel();
        let checkout = std::thread::spawn(move || {
            started.send(()).unwrap();
            waiting.get(1)
        });
        received.recv().unwrap();
        std::thread::sleep(Duration::from_millis(150));
        drop(state);
        assert!(checkout.join().unwrap().is_none());
    }
}
