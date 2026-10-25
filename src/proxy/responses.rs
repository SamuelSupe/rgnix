use super::{
    Context, Proxy, encode_path, error, expand, header_map, normalize_decoded_path, static_files,
};
use crate::model::Route;
use bytes::Bytes;
use pingora::{http::ResponseHeader, prelude::*};
use std::sync::Arc;
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
        route: &Arc<Route>,
    ) -> Result<bool> {
        if !["GET", "HEAD"].contains(&ctx.request.method.as_str()) {
            return self
                .reply(session, ctx, 405, "method not allowed\n".into(), None)
                .await;
        }
        let path = match &ctx.edits.path {
            Some(path) => normalize_decoded_path(path).map_err(|e| error(400, e.to_string()))?,
            None => ctx.request.path.clone(),
        };
        let opened_route = route.clone();
        let open_path = path.clone();
        let send_file = cfg!(target_os = "linux")
            && !self.tls
            && !session.is_http2()
            && route.settings.compression.gzip == 0
            && route.settings.compression.brotli == 0;
        #[cfg(target_os = "linux")]
        let cached = send_file
            .then(|| static_files::open_cached(&route.settings, &path))
            .flatten();
        #[cfg(not(target_os = "linux"))]
        let cached: Option<static_files::Opened> = None;
        let prefetch = !send_file
            && ctx.request.method == "GET"
            && !["range", "if-none-match", "if-modified-since"]
                .iter()
                .any(|name| ctx.request.headers.contains_key(name));
        let (opened, initial) = if let Some(opened) = cached {
            (opened, None)
        } else {
            tokio::task::spawn_blocking(move || {
                let mut opened = static_files::open_route(
                    &opened_route.settings,
                    &open_path,
                    opened_route.matcher.path(),
                )?;
                // A bounded small-file read shares the open's blocking job. Large
                // files and conditional/range requests retain streaming reads.
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
            .map_err(|e| {
                log::error!("static file: {e}");
                error(500, "file access failed")
            })?
        };
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
        let static_files::Selection {
            mut response,
            start,
            length,
        } = static_files::select(&file, &ctx.request.headers)?;
        let status = response.status.as_u16();
        self.response_headers(&mut response, ctx)?;
        if let Some(route) = &ctx.route {
            crate::compression::prepare(session, &response, &route.settings.compression);
        }
        let empty = ctx.request.method == "HEAD" || status == 304 || length == 0;
        session
            .write_response_header(Box::new(response), empty)
            .await?;
        if !empty {
            #[cfg(target_os = "linux")]
            if send_file
                && let Ok(length) = usize::try_from(length)
                && session
                    .downstream_session
                    .write_file_body(&file.file, start, length)
                    .await?
            {
                return Ok(true);
            }
            if let Some(initial) = initial {
                session.write_response_body(Some(initial), true).await?;
                return Ok(true);
            }
            let mut file = tokio::fs::File::from_std(file.file);
            if start != 0 {
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|e| error(500, e.to_string()))?;
            }
            let mut remaining = length;
            while remaining > 0 {
                let mut buffer = vec![0; remaining.min(65536) as usize];
                let n = file
                    .read(&mut buffer)
                    .await
                    .map_err(|e| error(500, e.to_string()))?;
                if n == 0 {
                    return Err(error(500, "file changed during transmission"));
                }
                remaining -= n as u64;
                buffer.truncate(n);
                session
                    .write_response_body(Some(Bytes::from(buffer)), remaining == 0)
                    .await?;
            }
        }
        Ok(true)
    }
}
