use super::{
    Listener,
    body::{Counters, Payload, RequestBody, ResponseBody},
    io::Connection,
};
use bytes::{Buf, Bytes};
use http::{HeaderValue, Request, Response, header};
use http_body_util::BodyExt;
use hyper::body::{Body, Incoming};
use std::sync::atomic::Ordering;
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

pub(super) async fn upgrade(
    listener: Arc<Listener>,
    mut request: Request<Incoming>,
    peer: SocketAddr,
    state: Arc<Connection>,
    mut shutdown: pingora::server::ShutdownWatch,
) -> Response<ResponseBody> {
    let attempt = prepare(&request);
    let settings = match attempt {
        Ok(settings) if !listener.config.tls && listener.config.http2 => settings,
        _ => return failure(state, "invalid h2c upgrade"),
    };
    let downstream = hyper::upgrade::on(&mut request);
    request.headers_mut().remove(header::CONNECTION);
    request.headers_mut().remove(header::UPGRADE);
    request.headers_mut().remove("http2-settings");
    let marker = format!("{:032x}", rand::random::<u128>());
    let initial = encode(&request, &settings, &marker);
    // HTTP/1 must consume its initial body before the socket changes framing.
    // Bound this exceptional negotiation buffer independently of unlimited routes.
    let permit = match listener.shared.requests.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return failure_status(state, 503, "h2c request budget exhausted"),
    };
    let snapshot = listener.shared.snapshot.load_full();
    let header_timeout = snapshot.header_timeout(listener.address);
    let route =
        crate::proxy::request_host_parts(request.headers(), request.uri(), request.version())
            .ok()
            .and_then(|host| snapshot.route(listener.address, &host, request.uri().path()));
    let timeout = route
        .as_ref()
        .map_or(Duration::from_secs(60), |r| r.settings.client_body_timeout);
    let max = route
        .as_ref()
        .map_or(1024 * 1024, |r| match r.settings.max_body {
            0 => 1024 * 1024,
            n => n.min(1024 * 1024),
        });
    if request.body().size_hint().lower() > max {
        return failure_status(state, 413, "h2c initial body exceeds limit");
    }
    let (mut parts, body) = request.into_parts();
    let counts = Arc::new(Counters::default());
    let collected = RequestBody::new(Payload::Incoming(body), counts.clone(), max, timeout)
        .collect()
        .await;
    let collected = match collected {
        Ok(collected) => collected,
        Err(_) => {
            return failure_status(
                state,
                match counts.request_error.load(Ordering::Relaxed) {
                    1 => 413,
                    3 => 408,
                    _ => 400,
                },
                "invalid h2c initial body",
            );
        }
    };
    drop(permit);
    parts.version = http::Version::HTTP_2;
    // Preserve trailers as well as bytes when replaying the bounded body.
    let trailers = collected.trailers().cloned();
    let bytes = collected.to_bytes();
    let frames = std::iter::once(Ok::<_, super::body::BoxError>(hyper::body::Frame::data(
        bytes,
    )))
    .chain(
        trailers
            .into_iter()
            .map(|h| Ok(hyper::body::Frame::trailers(h))),
    );
    let request = Request::from_parts(
        parts,
        Payload::Stream(Box::pin(http_body_util::StreamBody::new(
            futures::stream::iter(frames),
        ))),
    );
    let response = super::request::serve(&listener, request, peer, state.clone())
        .await
        .unwrap();
    if !response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    super::request::strip_hop_headers(&mut parts.headers);
    let response = Arc::new(Mutex::new(Some(Response::from_parts(parts, body))));
    let task_state = state.clone();
    let task = tokio::spawn(async move {
        let result = async {
            let mut stream = hyper_util::rt::TokioIo::new(downstream.await?);
            let mut preface = [0; 24];
            tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut preface)).await??;
            if &preface != b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" { return Err("invalid HTTP/2 preface".into()); }
            let stream = super::h2_headers::HeaderIo::new(
                Prefixed { prefix: Bytes::from(initial), stream }, header_timeout, task_state.failure.clone(),
            );
            let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
                let listener = listener.clone(); let state = task_state.clone();
                let first = if request.headers().get("x-rgnix-upgrade-initial").is_some_and(|v| v.as_bytes() == marker.as_bytes()) {
                    response.lock().unwrap_or_else(|e| e.into_inner()).take()
                } else { None };
                request.headers_mut().remove("x-rgnix-upgrade-initial");
                async move {
                    match first { Some(response) => Ok::<_, std::convert::Infallible>(response), None => super::request::serve(&listener, request.map(Payload::Incoming), peer, state).await }
                }
            });
            let mut builder = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new());
            builder.timer(hyper_util::rt::TokioTimer::new()).enable_connect_protocol();
            let connection = builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service);
            tokio::pin!(connection);
            tokio::select! {
                result = &mut connection => result?,
                _ = shutdown.changed() => { connection.as_mut().graceful_shutdown(); connection.await?; }
            }
            Ok::<_, super::body::BoxError>(())
        }.await;
        if let Err(error) = result {
            log::debug!("h2c upgrade: {error}");
        }
    });
    state.set_tunnel(task);
    let mut response = Response::new(ResponseBody::untracked(
        super::responses::reply(101, String::new(), None).into_body(),
        state,
    ));
    *response.status_mut() = http::StatusCode::SWITCHING_PROTOCOLS;
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    response
        .headers_mut()
        .insert(header::UPGRADE, HeaderValue::from_static("h2c"));
    response
}

struct Prefixed<T> {
    prefix: Bytes,
    stream: T,
}
impl<T: AsyncRead + Unpin> AsyncRead for Prefixed<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.prefix.has_remaining() {
            let n = self.prefix.len().min(buf.remaining());
            buf.put_slice(&self.prefix[..n]);
            self.prefix.advance(n);
            Poll::Ready(Ok(()))
        } else {
            Pin::new(&mut self.stream).poll_read(cx, buf)
        }
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for Prefixed<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, buf)
    }
    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

fn failure(state: Arc<Connection>, text: &str) -> Response<ResponseBody> {
    failure_status(state, 400, text)
}
fn failure_status(state: Arc<Connection>, status: u16, text: &str) -> Response<ResponseBody> {
    let response = super::responses::reply(status, text.into(), None);
    let (mut parts, body) = response.into_parts();
    parts
        .headers
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    Response::from_parts(parts, ResponseBody::untracked(body, state))
}

fn prepare(request: &Request<Incoming>) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        request.version() == http::Version::HTTP_11,
        "h2c requires HTTP/1.1"
    );
    crate::proxy::validate_connection_header(request.headers())?;
    let nominated: Vec<_> = request
        .headers()
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect();
    anyhow::ensure!(
        nominated.iter().any(|s| s.eq_ignore_ascii_case("upgrade"))
            && nominated
                .iter()
                .any(|s| s.eq_ignore_ascii_case("http2-settings")),
        "missing upgrade tokens"
    );
    let mut fields = request.headers().get_all("http2-settings").iter();
    let value = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("settings missing"))?
        .to_str()?;
    anyhow::ensure!(
        fields.next().is_none()
            && value.len() <= 256
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid settings encoding"
    );
    let mut encoded = value.replace('-', "+").replace('_', "/");
    while encoded.len() % 4 != 0 {
        encoded.push('=');
    }
    let decoded = openssl::base64::decode_block(&encoded)?;
    anyhow::ensure!(decoded.len() % 6 == 0, "invalid settings frame");
    Ok(decoded)
}

fn encode(request: &Request<Incoming>, settings: &[u8], marker: &str) -> Vec<u8> {
    let mut header = Vec::new();
    for (name, value) in [
        (":method", request.method().as_str()),
        (":scheme", "http"),
        (
            ":path",
            request.uri().path_and_query().map_or("/", |p| p.as_str()),
        ),
        (
            ":authority",
            request
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        ),
        ("x-rgnix-upgrade-initial", marker),
    ] {
        literal(&mut header, name.as_bytes(), value.as_bytes());
    }
    for (name, value) in request.headers() {
        if ![
            "host",
            "connection",
            "upgrade",
            "http2-settings",
            "transfer-encoding",
            "content-length",
            "keep-alive",
            "proxy-connection",
        ]
        .contains(&name.as_str())
        {
            literal(&mut header, name.as_str().as_bytes(), value.as_bytes());
        }
    }
    let mut bytes = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    frame(&mut bytes, 4, 0, 0, settings);
    let chunks: Vec<_> = header.chunks(16384).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        frame(
            &mut bytes,
            if i == 0 { 1 } else { 9 },
            (if i == 0 { 1 } else { 0 }) | (if i + 1 == chunks.len() { 4 } else { 0 }),
            1,
            chunk,
        );
    }
    bytes
}
fn literal(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    out.push(0);
    for bytes in [name, value] {
        let mut n = bytes.len();
        if n < 127 {
            out.push(n as u8);
        } else {
            out.push(127);
            n -= 127;
            while n >= 128 {
                out.push((n as u8 & 127) | 128);
                n >>= 7;
            }
            out.push(n as u8);
        }
        out.extend_from_slice(bytes);
    }
}
fn frame(out: &mut Vec<u8>, kind: u8, flags: u8, stream: u32, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(&[kind, flags]);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(bytes);
}
