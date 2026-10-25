use std::error::Error as StdError;
use std::future::Future;
use std::io::{Cursor, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Buf;
use futures_core::ready;
use h2::SendStream;
use http::header::{HeaderName, CONNECTION, TRANSFER_ENCODING, UPGRADE};
use http::HeaderMap;
use pin_project_lite::pin_project;

use crate::body::Body;

pub(crate) mod ping;
pub(crate) mod upgrade;

cfg_client! {
    pub(crate) mod client;
    pub(crate) use self::client::ClientTask;
}

cfg_server! {
    pub(crate) mod server;
    pub(crate) use self::server::Server;
}

/// Default initial stream window size defined in HTTP2 spec.
pub(crate) const SPEC_WINDOW_SIZE: u32 = 65_535;

// List of connection headers from RFC 9110 Section 7.6.1
//
// TE headers are allowed in HTTP/2 requests as long as the value is "trailers", so they're
// tested separately.
static CONNECTION_HEADERS: [HeaderName; 4] = [
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    TRANSFER_ENCODING,
    UPGRADE,
];

enum MessageKind {
    #[cfg(feature = "client")]
    Request,
    #[cfg(feature = "server")]
    Response,
}

fn strip_connection_headers(headers: &mut HeaderMap, kind: MessageKind) {
    for header in &CONNECTION_HEADERS {
        if headers.remove(header).is_some() {
            warn!("Connection header illegal in HTTP/2: {}", header.as_str());
        }
    }

    #[cfg(not(feature = "client"))]
    let _ = kind;
    #[cfg(feature = "client")]
    if matches!(kind, MessageKind::Request) {
        if headers
            .get(http::header::TE)
            .map_or(false, |te_header| te_header != "trailers")
        {
            warn!("TE headers not set to \"trailers\" are illegal in HTTP/2 requests");
            headers.remove(http::header::TE);
        }
    } else if headers.remove(http::header::TE).is_some() {
        warn!("TE headers illegal in HTTP/2 responses");
    }

    if let Some(header) = headers.remove(CONNECTION) {
        warn!(
            "Connection header illegal in HTTP/2: {}",
            CONNECTION.as_str()
        );
        // A `Connection` header may have a comma-separated list of names of other headers that
        // are meant for only this specific connection.
        //
        // Iterate these names and remove them as headers. Connection-specific headers are
        // forbidden in HTTP2, as that information has been moved into frame types of the h2
        // protocol.
        if let Ok(header_contents) = header.to_str() {
            for name in header_contents.split(',') {
                let name = name.trim();
                headers.remove(name);
            }
        }
    }
}

// body adapters used by both Client and Server

#[cfg(feature = "rgnix-extensions")]
mod timeout;
#[cfg(feature = "rgnix-extensions")]
use timeout::{BodyDeadline, BodyTimeout, HeaderTimeout};
#[cfg(not(feature = "rgnix-extensions"))]
type BodyTimeout = ();
#[cfg(not(feature = "rgnix-extensions"))]
type HeaderTimeout = ();
#[cfg(not(feature = "rgnix-extensions"))]
type BodyDeadline = ();

pin_project! {
    pub(crate) struct PipeToSendStream<S>
    where
        S: Body,
    {
        body_tx: SendStream<SendBuf<S::Data>>,
        data_done: bool,
        timeout: Option<BodyTimeout>,
        deadline: Option<BodyDeadline>,
        // A data chunk that has been polled from the body but is still waiting
        // for stream-level capacity before it can be shipped. Stored here so
        // it survives across `Poll::Pending` returns from `poll_capacity`; if
        // we left the chunk in a local, it would be dropped on every repoll.
        buffered_data: Option<Peeked<S::Data>>,
        #[pin]
        stream: S,
    }
}

struct Peeked<D> {
    data: D,
    is_eos: bool,
}

impl<S> PipeToSendStream<S>
where
    S: Body,
{
    fn new(stream: S, tx: SendStream<SendBuf<S::Data>>) -> PipeToSendStream<S> {
        PipeToSendStream {
            body_tx: tx,
            data_done: false,
            timeout: None,
            deadline: None,
            buffered_data: None,
            stream,
        }
    }

    #[cfg(feature = "rgnix-extensions")]
    fn with_timeout(mut self, timeout: Option<BodyTimeout>) -> Self {
        self.timeout = timeout;
        self
    }

    #[cfg(feature = "rgnix-extensions")]
    fn with_deadline(mut self, deadline: Option<crate::ext::H2Deadline>) -> Self {
        self.deadline = deadline.map(BodyDeadline::new);
        self
    }

    #[cfg(feature = "client")]
    fn send_reset(self: Pin<&mut Self>, reason: h2::Reason) {
        self.project().body_tx.send_reset(reason);
    }
}

impl<S> Future for PipeToSendStream<S>
where
    S: Body,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    type Output = crate::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut me = self.project();
        loop {
            #[cfg(feature = "rgnix-extensions")]
            if me.deadline.as_mut().is_some_and(|d| d.expired(cx)) {
                me.body_tx.send_reset(h2::Reason::CANCEL);
                return Poll::Ready(Err(crate::Error::new_body_write(std::io::Error::from(
                    std::io::ErrorKind::TimedOut,
                ))));
            }
            // Register for RST_STREAM notification while we wait for the next
            // body chunk or for send capacity, so the task wakes up if the
            // peer resets the stream.
            if let Poll::Ready(reason) = me
                .body_tx
                .poll_reset(cx)
                .map_err(crate::Error::new_body_write)?
            {
                debug!("stream received RST_STREAM: {:?}", reason);
                return Poll::Ready(Err(crate::Error::new_body_write(::h2::Error::from(reason))));
            }

            // If a previously-polled chunk is still waiting for stream-level
            // send capacity, drive that to completion before touching the
            // body again.
            if me.buffered_data.is_some() {
                while me.body_tx.capacity() == 0 {
                    let capacity = me.body_tx.poll_capacity(cx);
                    #[cfg(feature = "rgnix-extensions")]
                    if capacity.is_pending() && me.timeout.as_mut().is_some_and(|t| t.expired(cx)) {
                        me.body_tx.send_reset(h2::Reason::CANCEL);
                        return Poll::Ready(Err(crate::Error::new_body_write(
                            std::io::Error::from(std::io::ErrorKind::TimedOut),
                        )));
                    }
                    match ready!(capacity) {
                        Some(Ok(0)) => {}
                        Some(Ok(_)) => break,
                        Some(Err(e)) => return Poll::Ready(Err(crate::Error::new_body_write(e))),
                        None => {
                            // None means the stream is no longer in a
                            // streaming state, we either finished it
                            // somehow, or the remote reset us.
                            return Poll::Ready(Err(crate::Error::new_body_write(
                                "send stream capacity unexpectedly closed",
                            )));
                        }
                    }
                }

                #[cfg(feature = "rgnix-extensions")]
                if me.deadline.is_some() {
                    let peeked = me.buffered_data.as_mut().expect("checked is_some above");
                    let capacity = me.body_tx.capacity();
                    if capacity < peeked.data.remaining() {
                        // Keep unsendable bytes in this future, where its deadline
                        // remains active, rather than buffering the final frame in h2.
                        let part = peeked.data.copy_to_bytes(capacity);
                        me.body_tx
                            .send_data(SendBuf::Partial(part), false)
                            .map_err(crate::Error::new_body_write)?;
                        me.body_tx.reserve_capacity(peeked.data.remaining());
                        if let Some(timeout) = me.timeout.as_mut() {
                            timeout.progress();
                        }
                        continue;
                    }
                }
                let peeked = me.buffered_data.take().expect("checked is_some above");
                let buf = SendBuf::Buf(peeked.data);
                me.body_tx
                    .send_data(buf, peeked.is_eos)
                    .map_err(crate::Error::new_body_write)?;
                #[cfg(feature = "rgnix-extensions")]
                if let Some(timeout) = me.timeout {
                    timeout.progress();
                }

                if peeked.is_eos {
                    return Poll::Ready(Ok(()));
                }
                continue;
            }

            // Poll for the next body frame *before* reserving any connection
            // flow-control capacity. Reserving capacity speculatively (even a
            // single byte) pins that capacity on the connection-level window,
            // which can deadlock a second stream when talking to peers that
            // only emit WINDOW_UPDATE once their receive window is fully
            // exhausted. See #4003.
            match ready!(me.stream.as_mut().poll_frame(cx)) {
                Some(Ok(frame)) => {
                    if frame.is_data() {
                        let chunk = frame.into_data().unwrap_or_else(|_| unreachable!());
                        let is_eos = me.stream.is_end_stream();
                        let len = chunk.remaining();
                        trace!("send body chunk: {} bytes, eos={}", len, is_eos);

                        if len == 0 {
                            // Zero-length data frames need no capacity; send
                            // them straight through so trailing empty frames
                            // (e.g. an explicit end-of-stream marker) are
                            // delivered.
                            let buf = SendBuf::Buf(chunk);
                            me.body_tx
                                .send_data(buf, is_eos)
                                .map_err(crate::Error::new_body_write)?;

                            if is_eos {
                                return Poll::Ready(Ok(()));
                            }
                            continue;
                        }

                        // Ordinary bodies claim minimal capacity and let h2
                        // buffer the chunk. A deadline-bound response retains
                        // unsendable bytes here, so it requests the chunk's
                        // capacity itself. Neither path reserves before the
                        // body produces data (#4003).
                        #[cfg(feature = "rgnix-extensions")]
                        let capacity = if me.deadline.is_some() { len } else { 1 };
                        #[cfg(not(feature = "rgnix-extensions"))]
                        let capacity = 1;
                        me.body_tx.reserve_capacity(capacity);
                        *me.buffered_data = Some(Peeked {
                            data: chunk,
                            is_eos,
                        });
                    } else if frame.is_trailers() {
                        // no more DATA, so give any capacity back
                        me.body_tx.reserve_capacity(0);
                        me.body_tx
                            .send_trailers(frame.into_trailers().unwrap_or_else(|_| unreachable!()))
                            .map_err(crate::Error::new_body_write)?;
                        return Poll::Ready(Ok(()));
                    } else {
                        trace!("discarding unknown frame");
                        // loop again
                    }
                }
                Some(Err(e)) => return Poll::Ready(Err(me.body_tx.on_user_err(e))),
                None => {
                    // no more frames means we're done here
                    // but at this point, we haven't sent an EOS DATA, or
                    // any trailers, so send an empty EOS DATA.
                    return Poll::Ready(me.body_tx.send_eos_frame());
                }
            }
        }
    }
}

trait SendStreamExt {
    fn on_user_err<E>(&mut self, err: E) -> crate::Error
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>;
    fn send_eos_frame(&mut self) -> crate::Result<()>;
}

impl<B: Buf> SendStreamExt for SendStream<SendBuf<B>> {
    fn on_user_err<E>(&mut self, err: E) -> crate::Error
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let err = crate::Error::new_user_body(err);
        debug!("send body user stream error: {}", err);
        self.send_reset(err.h2_reason());
        err
    }

    fn send_eos_frame(&mut self) -> crate::Result<()> {
        trace!("send body eos");
        self.send_data(SendBuf::None, true)
            .map_err(crate::Error::new_body_write)
    }
}

#[repr(usize)]
enum SendBuf<B> {
    Buf(B),
    #[cfg(feature = "rgnix-extensions")]
    Partial(bytes::Bytes),
    Cursor(Cursor<Box<[u8]>>),
    None,
}

impl<B: Buf> Buf for SendBuf<B> {
    #[inline]
    fn remaining(&self) -> usize {
        match self {
            Self::Buf(b) => b.remaining(),
            #[cfg(feature = "rgnix-extensions")]
            Self::Partial(b) => b.remaining(),
            Self::Cursor(c) => Buf::remaining(c),
            Self::None => 0,
        }
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        match self {
            Self::Buf(b) => b.chunk(),
            #[cfg(feature = "rgnix-extensions")]
            Self::Partial(b) => b.chunk(),
            Self::Cursor(c) => c.chunk(),
            Self::None => &[],
        }
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        match self {
            Self::Buf(b) => b.advance(cnt),
            #[cfg(feature = "rgnix-extensions")]
            Self::Partial(b) => b.advance(cnt),
            Self::Cursor(c) => c.advance(cnt),
            Self::None => {}
        }
    }

    fn chunks_vectored<'a>(&'a self, dst: &mut [IoSlice<'a>]) -> usize {
        match self {
            Self::Buf(b) => b.chunks_vectored(dst),
            #[cfg(feature = "rgnix-extensions")]
            Self::Partial(b) => b.chunks_vectored(dst),
            Self::Cursor(c) => c.chunks_vectored(dst),
            Self::None => 0,
        }
    }
}
