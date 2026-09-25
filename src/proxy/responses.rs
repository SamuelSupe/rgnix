use super::{
    Context, Proxy, encode_path, error, expand, header_map, normalize_decoded_path, static_files,
};
use crate::model::Route;
use bytes::Bytes;
use pingora::{http::ResponseHeader, prelude::*};
use std::time::SystemTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

impl Proxy {
    pub(super) fn response_headers(
        &self,
        response: &mut ResponseHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        if response.status.is_informational() && response.status.as_u16() != 101 {
            return Ok(());
        }
        ctx.status = response.status.as_u16();
        if let Some(route) = &ctx.route {
            if let Some(policy) = &route.settings.gateway {
                policy.response_headers.response(response)?;
            }
            for header in &route.settings.response_headers {
                if header.always
                    || [200, 201, 204, 206, 301, 302, 303, 304, 307, 308].contains(&ctx.status)
                {
                    let value = expand(&header.value, ctx, self.tls, "");
                    if !value.is_empty() {
                        response.append_header(header.name.clone(), value)?;
                    }
                }
            }
        }
        if let Some(mut execution) = ctx.execution.take() {
            self.shared.telemetry.plugin_calls.inc();
            let result = {
                let _timer = self.shared.telemetry.plugin_duration.start_timer();
                execution.response(ctx.status, header_map(&response.headers))
            };
            drop(execution);
            ctx.plugin_permit.take();
            ctx.tenant_plugin.take();
            let edits = result.map_err(|e| {
                self.shared.telemetry.plugin_errors.inc();
                log::error!("response plugin: {e:#}");
                error(500, "response plugin failed")
            })?;
            for (key, value) in edits {
                if let Some(value) = value {
                    response.insert_header(key, value)?;
                } else {
                    response.remove_header(&key);
                }
            }
        }
        if let Some(cookie) = ctx.affinity_cookie.take() {
            response.append_header("Set-Cookie", cookie)?;
        }
        Ok(())
    }
    pub(super) async fn reply(
        &self,
        session: &mut Session,
        ctx: &mut Context,
        status: u16,
        body: String,
        location: Option<String>,
    ) -> Result<bool> {
        let body = if [204, 304].contains(&status) {
            String::new()
        } else {
            body
        };
        let mut response = ResponseHeader::build(status, None)?;
        response.insert_header("Content-Type", "text/plain; charset=utf-8")?;
        if status != 204 && status != 304 {
            response.insert_header("Content-Length", body.len().to_string())?;
        }
        if let Some(location) = location {
            response.insert_header("Location", location)?;
        }
        self.response_headers(&mut response, ctx)?;
        if let Some(route) = &ctx.route {
            crate::compression::prepare(session, &response, &route.settings.compression);
        }
        let head = ctx.request.method == "HEAD";
        let empty = body.is_empty() || head;
        session
            .write_response_header(Box::new(response), empty)
            .await?;
        if !empty {
            session
                .write_response_body(Some(Bytes::from(body)), true)
                .await?;
        }
        Ok(true)
    }
    pub(super) async fn serve_file(
        &self,
        session: &mut Session,
        ctx: &mut Context,
        route: &Route,
    ) -> Result<bool> {
        if !["GET", "HEAD"].contains(&ctx.request.method.as_str()) {
            return self
                .reply(session, ctx, 405, "method not allowed\n".into(), None)
                .await;
        }
        let path = ctx
            .edits
            .path
            .clone()
            .unwrap_or_else(|| ctx.request.path.clone());
        let path = normalize_decoded_path(&path).map_err(|e| error(400, e.to_string()))?;
        let settings = route.settings.clone();
        let open_path = path.clone();
        let prefix = route.matcher.path().to_owned();
        let opened = tokio::task::spawn_blocking(move || {
            static_files::open_route(&settings, &open_path, &prefix)
        })
        .await
        .map_err(|e| error(500, e.to_string()))?
        .map_err(|e| {
            log::error!("static file: {e}");
            error(500, "file access failed")
        })?;
        let file = match opened {
            static_files::Opened::Status(status) => {
                return self.reply(session, ctx, status, String::new(), None).await;
            }
            static_files::Opened::Directory => {
                let query = ctx.edits.query.as_deref().unwrap_or(&ctx.request.query);
                let query = if query.is_empty() {
                    String::new()
                } else {
                    format!("?{query}")
                };
                return self
                    .reply(
                        session,
                        ctx,
                        301,
                        String::new(),
                        Some(format!("{}/{query}", encode_path(&path))),
                    )
                    .await;
            }
            static_files::Opened::NotFound => {
                return self
                    .reply(session, ctx, 404, "not found\n".into(), None)
                    .await;
            }
            static_files::Opened::Forbidden => {
                return self
                    .reply(session, ctx, 403, "forbidden\n".into(), None)
                    .await;
            }
            static_files::Opened::File(file) => file,
        };
        let headers = &ctx.request.headers;
        let not_modified = if let Some(tag) = headers.get("if-none-match") {
            tag.split(',')
                .any(|s| s.trim() == "*" || s.trim().trim_start_matches("W/") == file.etag)
        } else {
            headers
                .get("if-modified-since")
                .and_then(|s| httpdate::parse_http_date(s).ok())
                .is_some_and(|date| {
                    file.modified
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        <= date
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                })
        };
        let mut status = if not_modified { 304 } else { 200 };
        let mut start = 0;
        let mut length = file.length;
        let mut content_range = None;
        let if_range = headers.get("if-range").is_none_or(|s| {
            s == &file.etag
                || httpdate::parse_http_date(s).ok().is_some_and(|d| {
                    file.modified
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        == d.duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                })
        });
        if !not_modified
            && if_range
            && let Some(range) = headers.get("range")
        {
            match static_files::range(range, file.length) {
                Ok(Some((a, b))) => {
                    status = 206;
                    start = a;
                    length = b - a + 1;
                    content_range = Some(format!("bytes {a}-{b}/{}", file.length));
                }
                Ok(None) => {}
                Err(()) => {
                    status = 416;
                    length = 0;
                    content_range = Some(format!("bytes */{}", file.length));
                }
            }
        }
        let mut response = ResponseHeader::build(status, None)?;
        response.insert_header("Content-Type", file.mime)?;
        response.insert_header("ETag", file.etag)?;
        response.insert_header("Last-Modified", httpdate::fmt_http_date(file.modified))?;
        response.insert_header("Accept-Ranges", "bytes")?;
        if status != 304 {
            response.insert_header("Content-Length", length.to_string())?;
        }
        if let Some(range) = content_range {
            response.insert_header("Content-Range", range)?;
        }
        self.response_headers(&mut response, ctx)?;
        if let Some(route) = &ctx.route {
            crate::compression::prepare(session, &response, &route.settings.compression);
        }
        let empty = ctx.request.method == "HEAD" || status == 304 || length == 0;
        session
            .write_response_header(Box::new(response), empty)
            .await?;
        if !empty {
            let mut file = tokio::fs::File::from_std(file.file);
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| error(500, e.to_string()))?;
            let mut buffer = vec![0; 65536];
            let mut remaining = length;
            while remaining > 0 {
                let count = buffer.len().min(remaining as usize);
                let n = file
                    .read(&mut buffer[..count])
                    .await
                    .map_err(|e| error(500, e.to_string()))?;
                if n == 0 {
                    return Err(error(500, "file changed during transmission"));
                }
                remaining -= n as u64;
                session
                    .write_response_body(Some(Bytes::copy_from_slice(&buffer[..n])), remaining == 0)
                    .await?;
            }
        }
        Ok(true)
    }
}
