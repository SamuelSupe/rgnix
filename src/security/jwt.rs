use anyhow::{Context, Result, ensure};
use arc_swap::ArcSwap;
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, JwkSet},
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize)]
pub struct Spec {
    pub source: String,
    pub issuer: String,
    pub audience: String,
    pub algorithm: String,
}
pub struct Jwt {
    pub spec: Spec,
    digest: String,
    keys: ArcSwap<Keys>,
    refresh: Mutex<(Instant, Instant)>,
}
struct Keys {
    algorithm: Algorithm,
    validation: Validation,
    keys: BTreeMap<String, DecodingKey>,
}
impl std::fmt::Debug for Jwt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jwt")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}
impl Serialize for Jwt {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serde_json::json!({"spec":self.spec,"sha256":self.digest}).serialize(serializer)
    }
}
impl Jwt {
    pub fn load(spec: Spec) -> Result<Arc<Self>> {
        let bytes = if spec.source.starts_with("https://") {
            let url = url::Url::parse(&spec.source)?;
            ensure!(
                url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
                "JWKS URL cannot contain credentials or fragments"
            );
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()?;
            let mut response = client.get(&spec.source).send()?.error_for_status()?;
            let mut bytes = Vec::new();
            std::io::Read::take(&mut response, 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            bytes
        } else {
            ensure!(!spec.source.contains("://"), "remote JWKS requires HTTPS");
            let file = std::fs::File::open(&spec.source)?;
            let mut bytes = Vec::new();
            std::io::Read::take(file, 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            bytes
        };
        Self::from_bytes(spec, &bytes)
    }
    pub fn from_bytes(spec: Spec, bytes: &[u8]) -> Result<Arc<Self>> {
        let keys = Keys::parse(&spec, bytes)?;
        let now = Instant::now();
        use sha2::Digest;
        Ok(Arc::new(Self {
            spec,
            digest: format!("{:x}", sha2::Sha256::digest(bytes)),
            keys: ArcSwap::from_pointee(keys),
            refresh: Mutex::new((now + Duration::from_secs(30), now)),
        }))
    }
    pub fn verify(&self, token: &str) -> Result<BTreeMap<String, String>> {
        ensure!(token.len() <= 16384, "JWT exceeds 16 KiB");
        if self.spec.source.starts_with("https://") {
            ensure!(
                self.refresh
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .1
                    .elapsed()
                    <= Duration::from_secs(300),
                "JWKS is stale"
            );
        }
        let header = decode_header(token)?;
        let keys = self.keys.load();
        ensure!(header.alg == keys.algorithm, "JWT algorithm mismatch");
        let key = header
            .kid
            .as_ref()
            .and_then(|kid| keys.keys.get(kid))
            .or_else(|| {
                if header.kid.is_none() && keys.keys.len() == 1 {
                    keys.keys.values().next()
                } else {
                    None
                }
            })
            .context("unknown JWT key")?;
        let decoded = decode::<BTreeMap<String, serde_json::Value>>(token, key, &keys.validation)?;
        ensure!(decoded.claims.len() <= 64, "too many JWT claims");
        let mut claims = BTreeMap::new();
        for (key, value) in decoded.claims {
            let value = match value {
                serde_json::Value::String(s) => s,
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            ensure!(
                key.len() <= 128 && value.len() <= 1024,
                "JWT claim too large"
            );
            claims.insert(key, value);
        }
        Ok(claims)
    }
    pub async fn refresh(&self) {
        if !self.spec.source.starts_with("https://") {
            return;
        }
        {
            let mut state = self.refresh.lock().unwrap_or_else(|e| e.into_inner());
            if Instant::now() < state.0 {
                return;
            }
            state.0 = Instant::now() + Duration::from_secs(30);
        }
        let result = async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()?;
            let mut response = client
                .get(&self.spec.source)
                .send()
                .await?
                .error_for_status()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    bytes.len() + chunk.len() <= 1024 * 1024,
                    "JWKS exceeds 1 MiB"
                );
                bytes.extend_from_slice(&chunk);
            }
            Keys::parse(&self.spec, &bytes)
        }
        .await;
        match result {
            Ok(keys) => {
                self.keys.store(Arc::new(keys));
                self.refresh.lock().unwrap_or_else(|e| e.into_inner()).1 = Instant::now();
            }
            Err(error) => {
                log::warn!("JWKS refresh failed; last keys expire after five minutes: {error}")
            }
        }
    }
}
impl Keys {
    fn parse(spec: &Spec, bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= 1024 * 1024, "JWKS exceeds 1 MiB");
        ensure!(
            !spec.issuer.is_empty()
                && spec.issuer.len() <= 1024
                && !spec.audience.is_empty()
                && spec.audience.len() <= 1024,
            "JWT requires bounded issuer and audience"
        );
        let algorithm = match spec.algorithm.as_str() {
            "RS256" => Algorithm::RS256,
            "ES256" => Algorithm::ES256,
            _ => anyhow::bail!("JWT algorithm must be RS256 or ES256"),
        };
        let mut validation = Validation::new(algorithm);
        validation.set_issuer(&[&spec.issuer]);
        validation.set_audience(&[&spec.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.validate_nbf = true;
        validation.leeway = 5;
        let set: JwkSet = serde_json::from_slice(bytes)?;
        ensure!(
            !set.keys.is_empty() && set.keys.len() <= 64,
            "JWKS requires 1..64 keys"
        );
        let mut keys = BTreeMap::new();
        for key in set.keys {
            if key
                .common
                .public_key_use
                .as_ref()
                .is_some_and(|v| *v != jsonwebtoken::jwk::PublicKeyUse::Signature)
            {
                continue;
            }
            let compatible = match &key.algorithm {
                AlgorithmParameters::RSA(rsa) => {
                    algorithm == Algorithm::RS256 && rsa.n.len() >= 342 && rsa.n.len() <= 1368
                }
                AlgorithmParameters::EllipticCurve(ec) => {
                    algorithm == Algorithm::ES256
                        && ec.curve == jsonwebtoken::jwk::EllipticCurve::P256
                }
                _ => false,
            };
            if !compatible {
                continue;
            }
            if let Some(declared) = key.common.key_algorithm {
                ensure!(
                    declared.to_string() == spec.algorithm,
                    "JWKS algorithm contradicts configured algorithm"
                );
            }
            let id = key.common.key_id.clone().unwrap_or_default();
            ensure!(
                id.len() <= 256 && !keys.contains_key(&id),
                "duplicate or oversized JWKS kid"
            );
            keys.insert(id, DecodingKey::from_jwk(&key)?);
        }
        ensure!(!keys.is_empty(), "JWKS has no usable signing keys");
        Ok(Self {
            algorithm,
            validation,
            keys,
        })
    }
}

use std::io::Read;
