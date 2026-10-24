//! Single-owner dispatch for a connection polled by its current request.
use super::*;

pin_project_lite::pin_project! {
    pub(crate) struct InlineClient<B> {
        request: Option<Request<B>>,
        response: Option<crate::Result<http::Response<IncomingBody>>>,
        awaiting: bool,
        ready: bool,
    }
}

impl<B> InlineClient<B> {
    fn new() -> Self {
        Self {
            request: None,
            response: None,
            awaiting: false,
            ready: true,
        }
    }
}

impl<B: Body> Dispatch for InlineClient<B> {
    type PollItem = RequestHead;
    type PollBody = B;
    type PollError = std::convert::Infallible;
    type RecvItem = crate::proto::ResponseHead;

    fn poll_msg(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<(RequestHead, B), Self::PollError>>> {
        let this = self.project();
        let Some(request) = this.request.take() else {
            *this.ready = true;
            return Poll::Pending;
        };
        *this.awaiting = true;
        let (parts, body) = request.into_parts();
        Poll::Ready(Some(Ok((
            RequestHead {
                version: parts.version,
                subject: crate::proto::RequestLine(parts.method, parts.uri),
                headers: parts.headers,
                extensions: parts.extensions,
            },
            body,
        ))))
    }

    fn recv_msg(
        &mut self,
        message: crate::Result<(Self::RecvItem, IncomingBody)>,
    ) -> crate::Result<()> {
        if !self.awaiting {
            // A failure before the queued request was encoded is still terminal;
            // this path never hands a request back for replay.
            if let Err(error) = message {
                if self.request.take().is_some() {
                    self.response = Some(Err(error));
                    return Ok(());
                }
                return Err(error);
            }
            return Err(crate::Error::new_unexpected_message());
        }
        self.awaiting = false;
        self.response = Some(message.map(|(head, body)| head.into_response(body)));
        Ok(())
    }

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        Poll::Ready(if self.awaiting { Ok(()) } else { Err(()) })
    }

    fn should_poll(&self) -> bool {
        !self.awaiting
    }
}

impl<B, I> Dispatcher<InlineClient<B>, B, I, super::super::ClientTransaction>
where
    I: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    pub(crate) fn inline_ready(&self) -> bool {
        !self.is_closing
            && self.conn.can_write_head()
            && self.dispatch.ready
            && !self.dispatch.awaiting
            && self.dispatch.request.is_none()
            && self.dispatch.response.is_none()
    }

    pub(crate) fn start_request(&mut self, request: Request<B>) -> crate::Result<()> {
        if !self.inline_ready() {
            return Err(crate::Error::new_canceled());
        }
        self.dispatch.ready = false;
        self.dispatch.request = Some(request);
        Ok(())
    }

    pub(crate) fn take_response(&mut self) -> Option<crate::Result<http::Response<IncomingBody>>> {
        self.dispatch.response.take()
    }
}

impl<B, I> Dispatcher<Client<B>, B, I, super::super::ClientTransaction>
where
    I: Read + Write + Unpin,
    B: Body + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    pub(crate) fn into_inline(
        self,
    ) -> Dispatcher<InlineClient<B>, B, I, super::super::ClientTransaction> {
        Dispatcher {
            conn: self.conn,
            dispatch: InlineClient::new(),
            body_tx: self.body_tx,
            body_rx: self.body_rx,
            is_closing: self.is_closing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::io::Compat;

    #[tokio::test]
    async fn inline_requests_reuse_only_after_response_framing() {
        for size in [1024, 8192, 8193, 16384] {
            let payload = vec![b'x'; size];
            let mut response =
                format!("HTTP/1.1 200 OK\r\ncontent-length: {size}\r\n\r\n").into_bytes();
            response.extend_from_slice(&payload);
            let io = tokio_test::io::Builder::new()
                .write(b"GET / HTTP/1.1\r\n\r\n")
                .read(&response)
                .write(b"GET / HTTP/1.1\r\n\r\n")
                .read(b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\nconnection: close\r\n\r\nend")
                .build();
            let (sender, connection) =
                crate::client::conn::http1::handshake::<_, IncomingBody>(Compat::new(io))
                    .await
                    .unwrap();
            let mut connection = connection.into_inline();
            drop(sender);
            for expected in [&payload[..], &b"end"[..]] {
                assert!(connection.is_ready());
                connection
                    .start_request(Request::new(IncomingBody::empty()))
                    .unwrap();
                assert!(!connection.is_ready());
                let response = std::future::poll_fn(|cx| connection.poll_response(cx))
                    .await
                    .unwrap();
                let mut body = response.into_body();
                let mut received = Vec::new();
                while let Some(frame) = std::future::poll_fn(|cx| {
                    let _ = Pin::new(&mut connection).poll(cx);
                    Pin::new(&mut body).poll_frame(cx)
                })
                .await
                {
                    received.extend_from_slice(&frame.unwrap().into_data().unwrap());
                }
                assert_eq!(received, expected);
                std::future::poll_fn(|cx| {
                    let _ = Pin::new(&mut connection).poll(cx);
                    if connection.is_ready() || connection.is_closed() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            }
            assert!(connection.is_closed());
            assert!(!connection.is_ready());
            assert!(connection
                .start_request(Request::new(IncomingBody::empty()))
                .is_err());
        }
    }
}
