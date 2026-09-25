use super::{Policy, Rate};
use crate::{script::RequestData, telemetry::Telemetry};
use anyhow::{Result, ensure};
use redis::aio::MultiplexedConnection;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};

// All decisions and cardinality changes use Redis time in one atomic operation.
// Expired fields are pruned in bounded batches; exhaustion rejects new keys.
const ADMIT: &str = r#"
local now_parts = redis.call('TIME')
local now = tonumber(now_parts[1])*1000 + math.floor(tonumber(now_parts[2])/1000)
local expired = redis.call('ZRANGEBYSCORE', KEYS[2], '-inf', now, 'LIMIT', 0, 128)
if #expired > 0 then
  redis.call('HDEL', KEYS[1], unpack(expired))
  redis.call('ZREM', KEYS[2], unpack(expired))
end
local id, rate, burst, capacity = ARGV[1], tonumber(ARGV[2]), tonumber(ARGV[3]), tonumber(ARGV[4])
local raw = redis.call('HGET', KEYS[1], id)
if not raw and redis.call('HLEN', KEYS[1]) >= capacity then return -1 end
local tokens, last = burst, now
if raw then
  local state = cjson.decode(raw)
  tokens, last = state[1], state[2]
end
tokens = math.min(burst, tokens + math.max(0, now-last)*rate/1000)
local allowed = 0
if tokens >= 1 then tokens = tokens-1; allowed = 1 end
local ttl = math.max(60000, math.ceil(burst/rate*1000)+1000)
redis.call('HSET', KEYS[1], id, cjson.encode({tokens, math.max(now,last)}))
redis.call('ZADD', KEYS[2], now+ttl, id)
local latest = redis.call('ZREVRANGE', KEYS[2], 0, 0, 'WITHSCORES')
local remaining = math.max(1, tonumber(latest[2])-now)
redis.call('PEXPIRE', KEYS[1], remaining)
redis.call('PEXPIRE', KEYS[2], remaining)
return allowed
"#;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Failure {
    #[default]
    Closed,
    Open,
    Local,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    url: String,
    scope: String,
    #[serde(default)]
    failure_mode: Failure,
    #[serde(default = "timeout")]
    timeout_ms: u64,
    #[serde(default = "concurrency")]
    max_inflight: usize,
}
fn timeout() -> u64 {
    100
}
fn concurrency() -> usize {
    256
}

pub struct Global {
    client: redis::Client,
    connection: Mutex<Option<MultiplexedConnection>>,
    permits: Semaphore,
    scope: String,
    timeout: Duration,
    failure: Failure,
}
impl Global {
    pub fn diagnostic(&self) -> serde_json::Value {
        serde_json::json!({"enabled":true,"scope":self.scope,"timeout_ms":self.timeout.as_millis(),
            "failure_mode":match self.failure {Failure::Closed=>"closed",Failure::Open=>"open",Failure::Local=>"local"}})
    }
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        let bytes = crate::controls::read_bounded(path, 64 * 1024)?;
        // Deserialization errors never include credential values.
        let config: Config = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid global rate limit configuration"))?;
        ensure!(
            (1..=2000).contains(&config.timeout_ms) && (1..=4096).contains(&config.max_inflight),
            "invalid shared limiter timeout or concurrency"
        );
        ensure!(
            !config.scope.is_empty()
                && config.scope.len() <= 128
                && config
                    .scope
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
            "shared limiter scope requires 1..128 letters, digits, dash, dot or underscore"
        );
        let url = url::Url::parse(&config.url).map_err(|_| anyhow::anyhow!("invalid Redis URL"))?;
        ensure!(
            matches!(url.scheme(), "redis" | "rediss")
                && url.host_str().is_some()
                && url.fragment().is_none()
                && url.query().is_none(),
            "Redis requires redis/rediss with no query or insecure fragment"
        );
        let client = redis::Client::open(config.url.as_str())
            .map_err(|_| anyhow::anyhow!("invalid Redis connection settings"))?;
        Ok(Arc::new(Self {
            client,
            connection: Mutex::new(None),
            permits: Semaphore::new(config.max_inflight),
            scope: config.scope,
            timeout: Duration::from_millis(config.timeout_ms),
            failure: config.failure_mode,
        }))
    }

    async fn admit(
        &self,
        group: &str,
        bucket: &str,
        rate: &Rate,
        value: &str,
        capacity: usize,
    ) -> Result<i32> {
        let _permit = self.permits.try_acquire()?;
        let mut connection = {
            let mut cached = self.connection.lock().await;
            if cached.is_none() {
                let config = redis::AsyncConnectionConfig::new()
                    .set_connection_timeout(Some(self.timeout))
                    .set_response_timeout(Some(self.timeout));
                *cached = Some(
                    self.client
                        .get_multiplexed_async_connection_with_config(&config)
                        .await?,
                );
            }
            cached.as_ref().unwrap().clone()
        };
        let group = format!("{:x}", Sha256::digest(format!("{}\0{group}", self.scope)));
        let bucket = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(bucket, rate, value))?)
        );
        let result = redis::cmd("EVAL")
            .arg(ADMIT)
            .arg(2)
            .arg(format!("rgnix:{{{group}}}:rates"))
            .arg(format!("rgnix:{{{group}}}:expiry"))
            .arg(bucket)
            .arg(rate.per_second)
            .arg(rate.burst)
            .arg(capacity)
            .query_async(&mut connection)
            .await;
        if result.is_err() {
            // Do not retry an ambiguous debit; reconnect for the next request.
            self.connection.lock().await.take();
        }
        Ok(result?)
    }

    async fn local(
        &self,
        group: &str,
        bucket: &str,
        rate: &Rate,
        value: &str,
        capacity: usize,
        telemetry: &Telemetry,
    ) -> std::result::Result<bool, u16> {
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            self.timeout,
            self.admit(group, bucket, rate, value, capacity),
        )
        .await;
        let (label, decision) = match result {
            Ok(Ok(1)) => ("allowed", Ok(false)),
            Ok(Ok(0)) => ("limited", Err(429)),
            Ok(Ok(-1)) => ("capacity", Err(503)),
            _ => {
                if let Ok(mut connection) = self.connection.try_lock() {
                    connection.take();
                }
                match self.failure {
                    Failure::Closed => ("unavailable_closed", Err(503)),
                    Failure::Open => ("unavailable_open", Ok(false)),
                    Failure::Local => ("unavailable_local", Ok(true)),
                }
            }
        };
        telemetry
            .traffic
            .shared_limit
            .with_label_values(&[label])
            .inc();
        telemetry
            .traffic
            .shared_limit_seconds
            .with_label_values(&[label])
            .observe(started.elapsed().as_secs_f64());
        decision
    }
}

pub async fn policy(
    global: Option<&Arc<Global>>,
    group: &str,
    bucket: &str,
    mut policy: Policy,
    request: &RequestData,
    capacity: usize,
    telemetry: &Telemetry,
) -> std::result::Result<Policy, u16> {
    if let (Some(global), Some(rate)) = (global, &policy.rate)
        && !global
            .local(
                group,
                bucket,
                rate,
                &rate.key.value(request, &request.claims),
                capacity,
                telemetry,
            )
            .await?
    {
        policy.rate = None;
    }
    Ok(policy)
}
