pub mod jwt;
pub mod mtls;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Security {
    pub jwt: Option<Arc<jwt::Jwt>>,
    pub external: Option<External>,
    pub mtls: mtls::Policy,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct External {
    pub url: String,
    pub endpoints: Vec<std::net::SocketAddr>,
    #[serde(skip)]
    pub pool: Option<Arc<crate::backend::Backend>>,

    pub timeout: Duration,
    pub response_headers: Vec<String>,
}
impl External {
    pub fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.url)?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "auth URL requires HTTP/HTTPS without credentials or fragments"
        );
        ensure!(
            !self.timeout.is_zero()
                && self.timeout <= Duration::from_secs(30)
                && self.response_headers.len() <= 16,
            "auth timeout must be 1ms..30s and at most 16 identity headers"
        );
        for header in &self.response_headers {
            http::header::HeaderName::from_bytes(header.as_bytes())?;
            ensure!(
                header.starts_with("x-")
                    && !header.starts_with("x-forwarded-")
                    && header != "x-real-ip",
                "auth identity headers must use a non-forwarding x- prefix"
            );
        }
        Ok(())
    }
    pub async fn authorize(
        &self,
        client: &reqwest::Client,
        request: &crate::script::RequestData,
        uri: &str,
    ) -> std::result::Result<BTreeMap<String, String>, u16> {
        let attempts = self.endpoints.len().clamp(1, 3);
        let deadline = tokio::time::Instant::now() + self.timeout;
        for attempt in 0..attempts {
            let lease = match &self.pool {
                Some(pool) => Some(pool.select("").ok_or(503u16)?),
                None => None,
            };
            let mut url = url::Url::parse(&self.url).map_err(|_| 503u16)?;
            if let Some(lease) = &lease {
                url.set_ip_host(lease.address.ip()).map_err(|_| 503u16)?;
                url.set_port(Some(lease.address.port()))
                    .map_err(|_| 503u16)?;
            }
            let mut check = client
                .get(url)
                .timeout(self.timeout)
                .header("X-Original-Method", &request.method)
                .header("X-Original-URI", uri)
                .header("X-Original-Host", &request.host)
                .header("X-Real-IP", &request.remote_addr);
            for name in ["authorization", "cookie"] {
                if let Some(value) = request.headers.get(name) {
                    if value.len() > 16384 {
                        return Err(400);
                    }
                    check = check.header(name, value);
                }
            }
            let result = tokio::time::timeout_at(deadline, check.send()).await;
            let response = match result {
                Ok(Ok(response)) => response,
                _ => {
                    if let Some(lease) = &lease {
                        self.pool.as_ref().unwrap().record_result(
                            lease.address,
                            true,
                            1,
                            Duration::from_secs(2),
                        );
                    }
                    if attempt + 1 < attempts && tokio::time::Instant::now() < deadline {
                        continue;
                    }
                    return Err(503);
                }
            };
            if response.status() == http::StatusCode::UNAUTHORIZED {
                return Err(401);
            }
            if response.status() == http::StatusCode::FORBIDDEN {
                return Err(403);
            }
            if !response.status().is_success() {
                if response.status().is_server_error()
                    && let Some(lease) = &lease
                {
                    self.pool.as_ref().unwrap().record_result(
                        lease.address,
                        true,
                        1,
                        Duration::from_secs(2),
                    );
                    if attempt + 1 < attempts && tokio::time::Instant::now() < deadline {
                        continue;
                    }
                }
                return Err(503);
            }
            let mut headers = BTreeMap::new();
            for name in &self.response_headers {
                if let Some(value) = response.headers().get(name) {
                    if value.len() > 4096 {
                        return Err(503);
                    }
                    headers.insert(name.clone(), value.to_str().map_err(|_| 503u16)?.to_owned());
                }
            }
            return Ok(headers);
        }
        Err(503)
    }
}
