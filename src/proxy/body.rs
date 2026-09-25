use super::error;
use crate::body::{BodyPolicy, BodyView, Inspection};
use bytes::Bytes;
use pingora::{ErrorType, Result, proxy::Session};

pub async fn inspect(
    session: &mut Session,
    policy: BodyPolicy,
    max_body: u64,
) -> Result<Option<BodyView>> {
    let (limit, full) = match policy.inspection {
        Inspection::Off => return Ok(None),
        Inspection::Full(n) => (n, true),
        Inspection::Prefix(n) => (n, false),
    };
    let length = session
        .req_header()
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if full && length.is_some_and(|n| n > limit as u64) {
        return Err(error(413, "request body exceeds full inspection limit"));
    }
    if session.is_body_done() {
        return Ok(Some(BodyView {
            bytes: Bytes::new(),
            complete: true,
        }));
    }
    // Reads return transport chunks, potentially exceeding the requested prefix.
    // Pingora replays this entire buffer on the first attempt, then streams the rest.
    if !session.enable_retry_buffering_with_limit(limit + 64 * 1024) {
        return Err(error(500, "cannot initialize request body replay buffer"));
    }
    let read = async {
        if session
            .req_header()
            .headers
            .get("expect")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"))
        {
            session
                .write_continue_response()
                .await
                .map_err(|e| e.into_down())?;
            session.req_header_mut().remove_header("expect");
        }
        let mut read_bytes = 0usize;
        let mut complete = false;
        loop {
            if session.is_body_done() {
                complete = true;
                break;
            }
            if !full && read_bytes >= limit {
                break;
            }
            let Some(chunk) = session
                .read_request_body()
                .await
                .map_err(|e| e.into_down())?
            else {
                complete = true;
                break;
            };
            read_bytes = read_bytes.saturating_add(chunk.len());
            if (max_body > 0 && read_bytes as u64 > max_body) || (full && read_bytes > limit) {
                return Err(error(
                    413,
                    "request body exceeds inspection or client body limit",
                ));
            }
            if session.retry_buffer_truncated() {
                return Err(error(500, "request body replay buffer exceeded"));
            }
        }
        let bytes = session.get_retry_buffer().unwrap_or_default();
        let visible = bytes.len().min(limit);
        // Copy only the visible slice so the Wasm instance does not retain an over-read tail.
        Ok(Some(BodyView {
            bytes: Bytes::copy_from_slice(&bytes[..visible]),
            complete: complete && read_bytes <= limit,
        }))
    };
    tokio::time::timeout(policy.timeout, read)
        .await
        .map_err(|_| error(408, "request body inspection timed out"))?
        .map_err(|e| {
            if e.etype() == &ErrorType::ReadTimedout {
                error(408, "request body inspection timed out")
            } else {
                e
            }
        })
}
