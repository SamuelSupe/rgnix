//! A worker-scoped HTTP/1 pool whose connection is driven by its current exchange.
//! It uses Hyper's framing and streaming implementation, never a second codec.
use super::{
    body::{BoxError, Payload, RequestBody},
    upstream::Connector,
};
use hyper::{
    Request, Response,
    body::{Body, Frame, Incoming, SizeHint},
    client::conn::http1,
};
use hyper_util::client::legacy::{IdlePoolBudget, pool::IdlePermit};
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tower_service::Service;

pub(super) type Observer = Arc<dyn Fn(bool, Duration) + Send + Sync>;
type Connection = Pin<Box<dyn Future<Output = hyper::Result<()>> + Send>>;

struct Exchange {
    sender: http1::SendRequest<RequestBody>,
    connection: Connection,
    address: SocketAddr,
}

struct Idle {
    exchange: Exchange,
    expires: Instant,
    _permit: IdlePermit,
}

type IdleBuckets = Vec<(SocketAddr, Vec<Idle>)>;

pub(super) struct Client {
    connector: Connector,
    observer: Observer,
    idle: Mutex<IdleBuckets>,
    budget: IdlePoolBudget,
    maximum: usize,
    keepalive: Duration,
    cleanup: OnceLock<tokio::task::AbortHandle>,
}

impl Client {
    pub fn new(
        connector: Connector,
        observer: Observer,
        budget: IdlePoolBudget,
        maximum: usize,
        keepalive: Duration,
    ) -> Arc<Self> {
        let client = Arc::new(Self {
            connector,
            observer,
            budget,
            maximum,
            keepalive,
            idle: Mutex::new(Vec::new()),
            cleanup: OnceLock::new(),
        });
        if maximum > 0 && !keepalive.is_zero() {
            let weak = Arc::downgrade(&client);
            let task = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(keepalive.max(Duration::from_millis(90))).await;
                    let Some(client) = weak.upgrade() else {
                        break;
                    };
                    let now = Instant::now();
                    let mut idle = client.idle.lock().unwrap_or_else(|e| e.into_inner());
                    for (_, entries) in idle.iter_mut() {
                        entries.retain(|e| e.expires > now);
                    }
                    idle.retain(|(_, entries)| !entries.is_empty());
                }
            });
            let _ = client.cleanup.set(task.abort_handle());
        }
        client
    }

    async fn checkout(
        &self,
        address: SocketAddr,
        uri: &hyper::Uri,
        now: Instant,
    ) -> Result<(Exchange, bool), Failure> {
        loop {
            let idle = {
                let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
                idle.iter()
                    .position(|(key, _)| *key == address)
                    .and_then(|i| idle[i].1.pop())
            };
            let Some(entry) = idle else {
                break;
            };
            if entry.expires <= now {
                continue;
            }
            let Idle { mut exchange, .. } = entry;
            // The connection has no request queued yet. Polling it detects a
            // buffered EOF or unsolicited response before any new bytes are sent.
            let open = futures::future::poll_fn(|cx| {
                Poll::Ready(exchange.connection.as_mut().poll(cx).is_pending())
            })
            .await;
            if open && exchange.sender.is_ready() {
                return Ok((exchange, true));
            }
        }
        let io = self
            .connector
            .clone()
            .call(uri.clone())
            .await
            .map_err(Failure::connect)?;
        let (sender, connection) = http1::handshake(io)
            .await
            .map_err(|e| Failure::connect(e.into()))?;
        Ok((
            Exchange {
                sender,
                connection: Box::pin(connection),
                address,
            },
            false,
        ))
    }

    fn put(&self, exchange: Exchange) {
        if self.maximum == 0 || self.keepalive.is_zero() || !exchange.sender.is_ready() {
            return;
        }
        let Some(expires) = Instant::now().checked_add(self.keepalive) else {
            return;
        };
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        let mut permit = self.budget.acquire();
        if permit.is_none() {
            // Match the legacy pool's eviction policy: replace this client's
            // oldest idle entry before giving up a full global reservation.
            let oldest = idle
                .iter()
                .enumerate()
                .flat_map(|(bucket, (_, entries))| {
                    entries
                        .iter()
                        .enumerate()
                        .map(move |(entry, value)| (bucket, entry, value.expires))
                })
                .min_by_key(|(_, _, time)| *time);
            if let Some((bucket, entry, _)) = oldest {
                idle[bucket].1.swap_remove(entry);
                permit = self.budget.acquire();
            }
        }
        let Some(permit) = permit else {
            return;
        };
        let address = exchange.address;
        let slot = idle
            .iter()
            .position(|(key, _)| *key == address)
            .or_else(|| idle.iter().position(|(_, entries)| entries.is_empty()));
        let entries = match slot {
            Some(i) => {
                idle[i].0 = address;
                &mut idle[i].1
            }
            None => {
                idle.push((address, Vec::new()));
                &mut idle.last_mut().unwrap().1
            }
        };
        if entries.len() < self.maximum {
            entries.push(Idle {
                exchange,
                expires,
                _permit: permit,
            });
        }
    }

    pub async fn request(
        self: &Arc<Self>,
        mut request: Request<RequestBody>,
        address: SocketAddr,
    ) -> Result<Response<Payload>, Failure> {
        let now = Instant::now();
        let (mut exchange, reused) = self.checkout(address, request.uri(), now).await?;
        (self.observer)(reused, now.elapsed());
        *request.uri_mut() = request.uri().path_and_query().cloned().map_or_else(
            || hyper::Uri::from_static("/"),
            |p| {
                hyper::Uri::builder()
                    .path_and_query(p)
                    .build()
                    .expect("validated origin-form URI")
            },
        );
        let response = exchange.sender.send_request(request);
        tokio::pin!(response);
        let mut completed = false;
        let response = futures::future::poll_fn(|cx| {
            if !completed {
                completed = exchange.connection.as_mut().poll(cx).is_ready();
            }
            response.as_mut().poll(cx)
        })
        .await
        .map_err(|e| Failure {
            error: e.into(),
            connect: false,
        })?;
        let (parts, body) = response.into_parts();
        // This is the same readiness invariant as legacy finish_response:
        // a ready sender has finished upstream framing. Buffered Incoming bytes
        // are independently owned and do not require the connection to advance.
        if !completed && exchange.sender.is_ready() {
            self.put(exchange);
            return Ok(Response::from_parts(parts, Payload::Incoming(body)));
        }
        let mut body = InlineBody {
            body,
            exchange: (!completed).then_some(exchange),
            pool: self.clone(),
        };
        if body.body.is_end_stream() {
            body.finish(None);
        }
        Ok(Response::from_parts(parts, Payload::Stream(Box::pin(body))))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.get() {
            cleanup.abort();
        }
    }
}

struct InlineBody {
    body: Incoming,
    exchange: Option<Exchange>,
    pool: Arc<Client>,
}
impl InlineBody {
    fn finish(&mut self, cx: Option<&mut Context<'_>>) {
        if let Some(mut exchange) = self.exchange.take() {
            let open = cx.is_none_or(|cx| exchange.connection.as_mut().poll(cx).is_pending());
            if open && exchange.sender.is_ready() {
                self.pool.put(exchange);
            }
        }
    }
}
impl Body for InlineBody {
    type Data = bytes::Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, BoxError>>> {
        let mut frame = Pin::new(&mut self.body)
            .poll_frame(cx)
            .map(|f| f.map(|r| r.map_err(Into::into)));
        if frame.is_pending() {
            if let Some(exchange) = &mut self.exchange
                && exchange.connection.as_mut().poll(cx).is_ready()
            {
                self.exchange = None;
            }
            frame = Pin::new(&mut self.body)
                .poll_frame(cx)
                .map(|f| f.map(|r| r.map_err(Into::into)));
        }
        if matches!(frame, Poll::Ready(Some(Err(_)))) {
            self.exchange = None;
        }
        if frame.is_ready() && self.body.is_end_stream() {
            self.finish(Some(cx));
        }
        frame
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
}

#[derive(Debug)]
pub(super) struct Failure {
    error: BoxError,
    connect: bool,
}
impl Failure {
    fn connect(error: BoxError) -> Self {
        Self {
            error,
            connect: true,
        }
    }
    pub fn is_connect(&self) -> bool {
        self.connect
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
