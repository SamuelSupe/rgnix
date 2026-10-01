use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use bytes::Bytes;
use hyper::{
    Request, Response,
    body::{Body, Frame, Incoming, SizeHint},
    client::conn::http1::{Connection, SendRequest},
};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type SessionSlot = Arc<Mutex<Option<Session>>>;

struct Session {
    sender: SendRequest<Incoming>,
    connection: Pin<Box<Connection<TokioIo<TcpStream>, Incoming>>>,
    closed: bool,
}

impl Session {
    async fn connect(address: SocketAddr) -> Result<Self, BoxError> {
        let socket = TcpStream::connect(address).await?;
        socket.set_nodelay(true)?;
        let (sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(socket)).await?;
        Ok(Self {
            sender,
            connection: Box::pin(connection),
            closed: false,
        })
    }

    fn drive(&mut self, cx: &mut Context<'_>) -> Result<(), BoxError> {
        if self.closed {
            return Ok(());
        }
        match self.connection.as_mut().poll(cx) {
            Poll::Pending => Ok(()),
            Poll::Ready(result) => {
                self.closed = true;
                result.map_err(Into::into)
            }
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct Paired {
    slot: SessionSlot,
}

impl Paired {
    pub fn poll_idle(&self, cx: &mut Context<'_>) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = slot.as_mut()
            && (session.drive(cx).is_err() || session.closed)
        {
            *slot = None;
        }
    }

    pub async fn request(
        &self,
        request: Request<Incoming>,
        upstream: SocketAddr,
    ) -> Result<Response<Payload>, BoxError> {
        let existing = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut session = match existing {
            Some(session) => session,
            None => Session::connect(upstream).await?,
        };
        let response = session.sender.send_request(request);
        tokio::pin!(response);
        let response = std::future::poll_fn(|cx| {
            // The connection has no separate task or shared foreground lock.
            // Dropping this future closes the session rather than replaying it.
            let failure = session.drive(cx).err();
            let result = response.as_mut().poll(cx).map_err(Into::into);
            if result.is_pending()
                && let Some(error) = failure
            {
                return Poll::Ready(Err(error));
            }
            if result.is_pending() && session.closed {
                return Poll::Ready(Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()));
            }
            result
        })
        .await?;
        Ok(response.map(|incoming| Payload {
            incoming,
            session: Some(session),
            slot: Some(self.slot.clone()),
        }))
    }
}

pub(super) struct Payload {
    incoming: Incoming,
    session: Option<Session>,
    slot: Option<SessionSlot>,
}

impl Payload {
    pub fn detached(incoming: Incoming) -> Self {
        Self {
            incoming,
            session: None,
            slot: None,
        }
    }
}

impl Body for Payload {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let failure = self
            .session
            .as_mut()
            .and_then(|session| session.drive(cx).err());
        let result = Pin::new(&mut self.incoming)
            .poll_frame(cx)
            .map(|frame| frame.map(|frame| frame.map_err(Into::into)));
        if matches!(result, Poll::Ready(Some(Err(_)))) {
            self.session = None;
        }
        // Hyper may have delivered headers or body frames before the driver
        // reports EOF. Drain those frames before surfacing the transport error.
        if result.is_pending()
            && let Some(error) = failure
        {
            self.session = None;
            return Poll::Ready(Some(Err(error)));
        }
        if result.is_pending() && self.session.as_ref().is_some_and(|s| s.closed) {
            return Poll::Ready(Some(Err(
                io::Error::from(io::ErrorKind::UnexpectedEof).into()
            )));
        }
        result
    }

    fn size_hint(&self) -> SizeHint {
        self.incoming.size_hint()
    }

    fn is_end_stream(&self) -> bool {
        self.incoming.is_end_stream()
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        if self.incoming.is_end_stream()
            && let Some(session) = self.session.take()
            && !session.closed
            && session.sender.is_ready()
            && let Some(slot) = &self.slot
        {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);
        }
    }
}
