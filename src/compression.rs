use pingora::{
    http::ResponseHeader, modules::http::compression::ResponseCompression,
    protocols::http::compression::Algorithm, proxy::Session,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Compression {
    pub gzip: u32,
    pub brotli: u32,
    pub min_length: u64,
    pub types: Vec<String>,
}
impl Default for Compression {
    fn default() -> Self {
        Self {
            gzip: 0,
            brotli: 0,
            min_length: 1024,
            types: vec![
                "text/html".into(),
                "text/plain".into(),
                "text/css".into(),
                "application/javascript".into(),
                "application/json".into(),
                "image/svg+xml".into(),
            ],
        }
    }
}
pub fn prepare(session: &mut Session, response: &ResponseHeader, policy: &Compression) {
    if response.status.is_informational() {
        return;
    }
    let mut req = session.req_header().clone();
    let encoding = negotiated_encoding(&req.headers, policy);
    let Some(compression) = session
        .downstream_modules_ctx
        .get_mut::<ResponseCompression>()
    else {
        return;
    };
    if !compression.is_header_phase() {
        return;
    }
    compression.adjust_level(0);
    let content_type = response
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let no_transform = response
        .headers
        .get_all("cache-control")
        .iter()
        .chain(req.headers.get_all("cache-control").iter())
        .filter_map(|v| v.to_str().ok())
        .any(|v| {
            v.split(',')
                .any(|d| d.trim().eq_ignore_ascii_case("no-transform"))
        });
    let small = response
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n < policy.min_length);
    if req.method == http::Method::HEAD
        || req.headers.contains_key("range")
        || response.status.as_u16() != 200
        || response.headers.contains_key("content-encoding")
        || response.headers.contains_key("content-range")
        || no_transform
        || small
        || content_type == "text/event-stream"
        || content_type.starts_with("application/grpc")
        || !policy
            .types
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&content_type))
    {
        return;
    }
    let Some(encoding) = encoding else {
        return;
    };
    // Pingora's parser ignores quality values. Pass only the negotiated coding
    // to the compression module, without changing the upstream request headers.
    req.insert_header("accept-encoding", encoding).unwrap();
    compression.adjust_algorithm_level(Algorithm::Gzip, policy.gzip);
    compression.adjust_algorithm_level(Algorithm::Brotli, policy.brotli);
    compression.request_filter(&req);
}

fn negotiated_encoding(headers: &http::HeaderMap, policy: &Compression) -> Option<&'static str> {
    let mut qualities: [Option<u16>; 4] = [None; 4];
    let mut bytes = 0;
    for value in headers.get_all("accept-encoding") {
        bytes += value.len();
        if bytes > 8192 {
            return None;
        }
        for item in value.to_str().ok()?.split(',') {
            let mut parts = item.trim().split(';');
            let name = parts.next()?.trim();
            if name.is_empty() {
                continue;
            }
            let quality = match parts.next() {
                None => 1000,
                Some(parameter) => {
                    let (key, value) = parameter.trim().split_once('=')?;
                    if !key.eq_ignore_ascii_case("q") || parts.next().is_some() {
                        return None;
                    }
                    quality(value.trim())?
                }
            };
            let index = if name.eq_ignore_ascii_case("br") {
                0
            } else if name.eq_ignore_ascii_case("gzip") {
                1
            } else if name == "*" {
                2
            } else if name.eq_ignore_ascii_case("identity") {
                3
            } else {
                continue;
            };
            qualities[index] = Some(qualities[index].map_or(quality, |old| old.min(quality)));
        }
    }
    let wildcard = qualities[2].unwrap_or(0);
    let br = if policy.brotli > 0 {
        qualities[0].unwrap_or(wildcard)
    } else {
        0
    };
    let gzip = if policy.gzip > 0 {
        qualities[1].unwrap_or(wildcard)
    } else {
        0
    };
    let best = br.max(gzip);
    if best == 0 || qualities[3].is_some_and(|identity| identity > best) {
        None
    } else if br >= gzip {
        Some("br")
    } else {
        Some("gzip")
    }
}

fn quality(value: &str) -> Option<u16> {
    let (integer, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match integer {
        "0" => Some(fraction.parse::<u16>().unwrap_or(0) * 10_u16.pow(3 - fraction.len() as u32)),
        "1" if fraction.bytes().all(|b| b == b'0') => Some(1000),
        _ => None,
    }
}
