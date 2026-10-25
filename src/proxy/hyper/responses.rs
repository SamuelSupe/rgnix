use super::body::{BoxError, Payload};
use crate::proxy::{Context, Proxy, error, static_files};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    Response,
    body::{Body, Frame, SizeHint},
};
use pingora::{
    http::{RequestHeader, ResponseHeader},
    protocols::http::compression::{Algorithm, ResponseCompressionCtx},
};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub(super) fn reply(status: u16, text: String, location: Option<String>) -> Response<Payload> {
    let text = if [204, 304].contains(&status) {
        String::new()
    } else {
        text
    };
    let length = text.len();
    let mut response = Response::new(Payload::Full(Full::new(Bytes::from(text))));
    *response.status_mut() = http::StatusCode::from_u16(status).unwrap();
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if ![204, 304].contains(&status) {
        response
            .headers_mut()
            .insert(http::header::CONTENT_LENGTH, length.into());
    }
    if let Some(location) = location
        && let Ok(value) = location.parse()
    {
        response.headers_mut().insert(http::header::LOCATION, value);
    }
    response
}

pub(super) fn prepare(
    proxy: &Proxy,
    ctx: &mut Context,
    response: &mut Response<Payload>,
) -> pingora::Result<()> {
    ctx.status = response.status().as_u16();
    if let Some(route) = &ctx.route
        && ctx.execution.is_none()
        && ctx.affinity_cookie.is_none()
        && route.settings.gateway.is_none()
        && route.settings.compression.gzip == 0
        && route.settings.compression.brotli == 0
    {
        let prepared = ctx
            .snapshot
            .as_ref()
            .unwrap()
            .hyper
            .as_ref()
            .unwrap()
            .route(proxy.listener, route);
        for (name, value, always) in &prepared.response_headers {
            if *always || [200, 201, 204, 206, 301, 302, 303, 304, 307, 308].contains(&ctx.status) {
                let value = value.expand(ctx, "")?;
                if !value.is_empty() {
                    response.headers_mut().append(name.clone(), value);
                }
            }
        }
        return Ok(());
    }
    let empty = Response::new(Payload::Full(Full::new(Bytes::new())));
    let (parts, mut body) = std::mem::replace(response, empty).into_parts();
    let mut head = ResponseHeader::from(parts);
    proxy.response_headers(&mut head, ctx)?;
    let policy = ctx.route.as_ref().map(|r| &r.settings.compression);
    if let Some(policy) = policy.filter(|p| p.gzip > 0 || p.brotli > 0) {
        let headers: http::HeaderMap = ctx
            .request
            .headers
            .iter()
            .filter_map(|(k, v)| {
                Some((
                    k.parse::<http::HeaderName>().ok()?,
                    v.parse::<http::HeaderValue>().ok()?,
                ))
            })
            .collect();
        let method = ctx.request.method.parse().unwrap_or(http::Method::GET);
        if let Some(encoding) =
            crate::compression::selected_encoding(&method, &headers, &head, policy)
        {
            let mut compression = ResponseCompressionCtx::new(0, false, false);
            compression.adjust_algorithm_level(Algorithm::Gzip, policy.gzip);
            compression.adjust_algorithm_level(Algorithm::Brotli, policy.brotli);
            let mut request = RequestHeader::build(method.as_str(), b"/", None)?;
            request.insert_header("accept-encoding", encoding)?;
            compression.request_filter(&request);
            compression.response_header_filter(&mut head, body.is_end_stream());
            if compression.is_enabled() {
                let inner = std::mem::replace(&mut body, Payload::Full(Full::new(Bytes::new())));
                body = Payload::Stream(Box::pin(Encoded {
                    inner,
                    compression,
                    trailer: None,
                    ended: false,
                }));
            }
        }
    }
    *response = Response::from_parts(head.as_owned_parts(), body);
    Ok(())
}

pub(super) async fn file(
    ctx: &Context,
    route: Arc<crate::model::Route>,
    sendfile: bool,
) -> pingora::Result<Response<Payload>> {
    if !["GET", "HEAD"].contains(&ctx.request.method.as_str()) {
        return Ok(reply(405, "method not allowed\n".into(), None));
    }
    let path = match &ctx.edits.path {
        Some(path) => {
            crate::proxy::normalize_decoded_path(path).map_err(|e| error(400, e.to_string()))?
        }
        None => ctx.request.path.clone(),
    };
    let open_path = path.clone();
    let prefetch = !sendfile
        && ctx.request.method == "GET"
        && !["range", "if-none-match", "if-modified-since"]
            .iter()
            .any(|name| ctx.request.headers.contains_key(name));
    let (opened, initial) = tokio::task::spawn_blocking(move || {
        let mut opened =
            static_files::open_route(&route.settings, &open_path, route.matcher.path())?;
        let initial = if prefetch
            && let static_files::Opened::File(file) = &mut opened
            && file.length <= 65536
        {
            let mut buffer = vec![0; file.length as usize];
            std::io::Read::read_exact(&mut file.file, &mut buffer)?;
            Some(Bytes::from(buffer))
        } else {
            None
        };
        Ok::<_, std::io::Error>((opened, initial))
    })
    .await
    .map_err(|e| error(500, e.to_string()))?
    .map_err(|_| error(500, "file access failed"))?;
    let file = match opened {
        static_files::Opened::File(file) => file,
        static_files::Opened::Status(status) => return Ok(reply(status, String::new(), None)),
        static_files::Opened::NotFound => return Ok(reply(404, "not found\n".into(), None)),
        static_files::Opened::Forbidden => return Ok(reply(403, "forbidden\n".into(), None)),
        static_files::Opened::Directory => {
            let query = ctx.edits.query.as_deref().unwrap_or(&ctx.request.query);
            let query = if query.is_empty() {
                String::new()
            } else {
                format!("?{query}")
            };
            return Ok(reply(
                301,
                String::new(),
                Some(format!("{}/{query}", crate::proxy::encode_path(&path))),
            ));
        }
    };
    let selected = static_files::select(&file, &ctx.request.headers)?;
    let empty = ctx.request.method == "HEAD"
        || selected.response.status == http::StatusCode::NOT_MODIFIED
        || selected.length == 0;
    let body = if empty {
        Payload::Full(Full::new(Bytes::new()))
    } else if cfg!(target_os = "linux") && sendfile {
        #[cfg(target_os = "linux")]
        {
            Payload::SendFile(hyper::ext::SendFile {
                file: Arc::new(file.file),
                offset: selected.start,
                length: selected.length,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            unreachable!()
        }
    } else if let Some(initial) = initial {
        Payload::Full(Full::new(initial))
    } else {
        let mut file = tokio::fs::File::from_std(file.file);
        if selected.start > 0 {
            file.seek(std::io::SeekFrom::Start(selected.start))
                .await
                .map_err(|e| error(500, e.to_string()))?;
        }
        let stream = futures::stream::try_unfold(
            (file, selected.length),
            |(mut file, remaining)| async move {
                if remaining == 0 {
                    return Ok::<_, BoxError>(None);
                }
                let mut buffer = vec![0; remaining.min(65536) as usize];
                let n = file.read(&mut buffer).await?;
                if n == 0 {
                    return Err("file changed during transmission".into());
                }
                buffer.truncate(n);
                Ok(Some((
                    Frame::data(Bytes::from(buffer)),
                    (file, remaining - n as u64),
                )))
            },
        );
        Payload::Stream(Box::pin(FileBody {
            inner: Box::pin(http_body_util::StreamBody::new(stream)),
            remaining: selected.length,
        }))
    };
    let mut response = Response::new(body);
    *response.status_mut() = selected.response.status;
    *response.headers_mut() = selected.response.headers.clone();
    Ok(response)
}

struct FileBody {
    inner: Pin<Box<dyn Body<Data = Bytes, Error = BoxError> + Send>>,
    remaining: u64,
}
impl Body for FileBody {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let frame = std::task::ready!(self.inner.as_mut().poll_frame(cx));
        if let Some(Ok(frame)) = &frame
            && let Some(data) = frame.data_ref()
        {
            self.remaining = self.remaining.saturating_sub(data.len() as u64);
        }
        Poll::Ready(frame)
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }
}

struct Encoded {
    inner: Payload,
    compression: ResponseCompressionCtx,
    trailer: Option<Frame<Bytes>>,
    ended: bool,
}
impl Body for Encoded {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some(trailer) = self.trailer.take() {
            return Poll::Ready(Some(Ok(trailer)));
        }
        if self.ended {
            return Poll::Ready(None);
        }
        loop {
            let frame = std::task::ready!(Pin::new(&mut self.inner).poll_frame(cx));
            let data = match frame {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => Some(data),
                    Err(trailer) => {
                        self.trailer = Some(trailer);
                        self.ended = true;
                        None
                    }
                },
                Some(Err(error)) => {
                    self.ended = true;
                    return Poll::Ready(Some(Err(error)));
                }
                None => {
                    self.ended = true;
                    None
                }
            };
            let ended = self.ended;
            let encoded = self.compression.response_body_filter(data.as_ref(), ended);
            if !self.compression.is_enabled() {
                self.ended = true;
                return Poll::Ready(Some(Err("response compression failed".into())));
            }
            if let Some(encoded) = encoded.filter(|b| !b.is_empty()) {
                return Poll::Ready(Some(Ok(Frame::data(encoded))));
            }
            if self.ended {
                return Poll::Ready(self.trailer.take().map(Ok));
            }
        }
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
    fn is_end_stream(&self) -> bool {
        self.ended && self.trailer.is_none()
    }
}
