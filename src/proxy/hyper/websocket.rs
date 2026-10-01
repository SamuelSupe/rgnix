use super::{body::RequestBody, request::strip_hop_headers};
use crate::{proxy::error, upstream::Protocol};
use http::{HeaderValue, Request, Response, header};
use hyper::body::Incoming;

pub(super) struct Handshake {
    extended: bool,
    upstream_h2: bool,
    key: Option<HeaderValue>,
}
impl Handshake {
    pub fn prepare(
        request: &mut Request<RequestBody>,
        extended: bool,
        protocol: &Protocol,
    ) -> pingora::Result<Self> {
        let mut websocket_key = None;
        if *protocol == Protocol::Http2 {
            *request.method_mut() = http::Method::CONNECT;
            request
                .extensions_mut()
                .insert(hyper::ext::Protocol::from_static("websocket"));
            websocket_key = request.headers_mut().remove("sec-websocket-key");
        } else {
            if extended {
                *request.method_mut() = http::Method::GET;
                let mut key = [0; 16];
                openssl::rand::rand_bytes(&mut key).map_err(|e| error(500, e.to_string()))?;
                let key = http::HeaderValue::from_str(&openssl::base64::encode_block(&key))
                    .map_err(|e| error(500, e.to_string()))?;
                request
                    .headers_mut()
                    .insert("sec-websocket-key", key.clone());
                websocket_key = Some(key);
            }
            request.headers_mut().insert(
                header::CONNECTION,
                http::HeaderValue::from_static("upgrade"),
            );
            request
                .headers_mut()
                .insert(header::UPGRADE, http::HeaderValue::from_static("websocket"));
        }
        Ok(Self {
            extended,
            upstream_h2: *protocol == Protocol::Http2,
            key: websocket_key,
        })
    }
    pub fn accept(&self, response: &mut Response<Incoming>) -> pingora::Result<bool> {
        let upstream_websocket = if self.upstream_h2 {
            response.status() == http::StatusCode::OK
        } else {
            response.status() == http::StatusCode::SWITCHING_PROTOCOLS
        };
        if upstream_websocket {
            if !self.upstream_h2 {
                if !response
                    .headers()
                    .get(header::UPGRADE)
                    .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
                {
                    return Err(error(502, "invalid upstream upgrade"));
                }
                if self.extended
                    && response.headers().get("sec-websocket-accept")
                        != self
                            .key
                            .as_ref()
                            .map(websocket_accept)
                            .transpose()?
                            .as_ref()
                {
                    return Err(error(502, "invalid upstream WebSocket accept"));
                }
            }
            if self.extended {
                *response.status_mut() = http::StatusCode::OK;
                strip_hop_headers(response.headers_mut());
                response.headers_mut().remove("sec-websocket-accept");
            } else if self.upstream_h2 {
                *response.status_mut() = http::StatusCode::SWITCHING_PROTOCOLS;
                response.headers_mut().insert(
                    header::CONNECTION,
                    http::HeaderValue::from_static("upgrade"),
                );
                response
                    .headers_mut()
                    .insert(header::UPGRADE, http::HeaderValue::from_static("websocket"));
                let key = self
                    .key
                    .as_ref()
                    .ok_or_else(|| error(400, "WebSocket key missing"))?;
                response
                    .headers_mut()
                    .insert("sec-websocket-accept", websocket_accept(key)?);
            }
        }
        Ok(upstream_websocket)
    }
}

fn websocket_accept(key: &http::HeaderValue) -> pingora::Result<http::HeaderValue> {
    let mut value = key.as_bytes().to_vec();
    value.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    http::HeaderValue::from_str(&openssl::base64::encode_block(&openssl::sha::sha1(&value)))
        .map_err(|e| error(502, e.to_string()))
}
