use super::*;
use crate::traffic::Limiter;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthFixture {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Simulation {
    pub listener: Option<SocketAddr>,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub host: String,
    pub path: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_client")]
    pub client: String,
    #[serde(default)]
    pub body: String,
    pub client_certificate: Option<String>,
    pub external_auth: Option<AuthFixture>,
    #[serde(default = "one")]
    pub repeat: usize,
    #[serde(default)]
    pub hold_permits: bool,
}
impl Simulation {
    pub(crate) fn routing_request(&self) -> Result<RequestData> {
        let (path, query) = self.path.split_once('?').unwrap_or((&self.path, ""));
        http::Method::from_bytes(self.method.as_bytes())?;
        let mut headers = BTreeMap::new();
        for (name, value) in &self.headers {
            let name = http::header::HeaderName::from_bytes(name.as_bytes())?;
            http::HeaderValue::from_str(value)?;
            headers.insert(name.as_str().into(), value.clone());
        }
        if !headers.contains_key("host") && !self.host.is_empty() {
            http::HeaderValue::from_str(&self.host)?;
            headers.insert("host".into(), self.host.clone());
        }
        Ok(RequestData {
            method: self.method.clone(),
            host: self.host.clone(),
            path: crate::proxy::normalized_path(path)?,
            query: query.into(),
            headers,
            remote_addr: self.client.clone(),
            ..Default::default()
        })
    }
}
fn default_method() -> String {
    "GET".into()
}
fn default_client() -> String {
    "127.0.0.1".into()
}
fn one() -> usize {
    1
}
pub async fn simulate(
    snapshot: &RuntimeSnapshot,
    input: Simulation,
    live_auth: bool,
) -> Result<Value> {
    ensure!(
        input.path.len() <= 8192
            && input.body.len() <= crate::body::MAX_INSPECT_BYTES
            && (1..=1000).contains(&input.repeat),
        "simulation input exceeds URI/body/repeat limits"
    );
    let listener = input
        .listener
        .or_else(|| snapshot.listeners.first().map(|l| l.address))
        .ok_or_else(|| anyhow::anyhow!("missing listener"))?;
    let mut request = input.routing_request()?;
    let path = request.path.clone();
    let peer = input.client.parse()?;
    let mut headers = http::HeaderMap::new();
    for (name, value) in &input.headers {
        headers.insert(
            http::header::HeaderName::from_bytes(name.as_bytes())?,
            http::header::HeaderValue::from_str(value)?,
        );
    }
    if !headers.contains_key("host") && !input.host.is_empty() {
        headers.insert("host", http::HeaderValue::from_str(&input.host)?);
    }
    request.headers = headers
        .iter()
        .map(|(k, v)| Ok((k.as_str().to_owned(), v.to_str()?.to_owned())))
        .collect::<Result<_>>()?;
    let Some(route) = snapshot.route_request(listener, &request) else {
        return Ok(json!({"status":404,"matched":false}));
    };
    let client = route.settings.identity.resolve(peer, &headers, None);
    request.remote_addr = client.to_string();
    let certificate = input
        .client_certificate
        .as_ref()
        .map(|pem| -> Result<crate::security::mtls::Peer> {
            let mut certs = openssl::x509::X509::stack_from_pem(pem.as_bytes())?;
            ensure!(!certs.is_empty(), "empty client certificate");
            let leaf = certs.remove(0);
            let mut chain = openssl::stack::Stack::new()?;
            for cert in certs {
                chain.push(cert)?;
            }
            Ok(crate::security::mtls::Peer { leaf, chain })
        })
        .transpose()?;
    let limiter = Arc::new(Limiter::default());
    let mut permits = Vec::new();
    let mut results = Vec::new();
    let auth_client = crate::upstream::Transport::default()
        .client_builder()?
        .build()?;
    let tls = snapshot
        .listeners
        .iter()
        .any(|l| l.address == listener && l.tls);
    let scheme = route
        .settings
        .identity
        .scheme(peer, &headers, tls)
        .to_owned();
    for sample in 0..input.repeat {
        let mut run = request.clone();
        if scheme != "https"
            && route.settings.identity.allows(client)
            && let Some(port) = route.settings.https_redirect_port
        {
            results.push(json!({"admitted":true,"status":308,"outbound":{"kind":"redirect","location":crate::proxy::https_redirect(&input.host,&input.path,port)}}));
            continue;
        }
        let mut held = Vec::new();
        let admitted = async {
            if !route.settings.identity.allows(client)
                || !route.settings.security.mtls.authorize(certificate.as_ref())
            {
                return Err(403u16);
            }
            if route.settings.max_body > 0 && input.body.len() as u64 > route.settings.max_body {
                return Err(413);
            }
            if let Some(quota) = route.tenant.as_ref().map(|t| t.quota.load_full()) {
                if input.hold_permits
                    && results
                        .iter()
                        .filter(|v: &&Value| v["admitted"] == true)
                        .count()
                        >= quota.max_inflight
                {
                    return Err(503);
                }
                limiter.acquire(
                    "_namespace",
                    &crate::traffic::Policy {
                        rate: Some(crate::traffic::Rate {
                            per_second: quota.requests_per_second,
                            burst: quota.burst,
                            key: crate::traffic::Key::Route,
                        }),
                        concurrency: None,
                    },
                    &run,
                    &run.claims,
                )?;
            }
            held.extend(limiter.acquire(
                &route.id,
                &route.settings.traffic.phase(true),
                &run,
                &run.claims,
            )?);
            if let Some(jwt) = &route.settings.security.jwt {
                let token = run
                    .headers
                    .get("authorization")
                    .and_then(|s| s.strip_prefix("Bearer "))
                    .ok_or(401u16)?;
                run.claims = jwt.verify(token).map_err(|_| 401u16)?;
            }
            if let Some(auth) = &route.settings.security.external {
                for name in &auth.response_headers {
                    run.headers.remove(name);
                }
                let identity = if live_auth {
                    auth.authorize(&auth_client, &run, &input.path, None, None)
                        .await?
                } else if let Some(fixture) = &input.external_auth {
                    match fixture.status {
                        200..=299 => {}
                        401 | 403 => return Err(fixture.status),
                        _ => return Err(503),
                    }
                    fixture
                        .headers
                        .iter()
                        .filter(|(k, _)| auth.response_headers.contains(&k.to_ascii_lowercase()))
                        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                        .collect()
                } else {
                    return Err(424);
                };
                run.headers.extend(identity);
            }
            held.extend(limiter.acquire(
                &route.id,
                &route.settings.traffic.phase(false),
                &run,
                &run.claims,
            )?);
            Ok(())
        }
        .await;
        let mut result = json!({"route":route.id,"action":route.action,"normalized_path":path,"mode":"isolated admission and RGL simulation; upstream I/O is not executed","external_auth":if live_auth {"live"} else {"fixture"}});
        if let Err(status) = admitted {
            result["status"] = json!(status);
            result["admitted"] = json!(false);
            if status == 424 {
                result["error"] = json!(
                    "external_auth fixture required; use CLI --live-auth for a real auth call"
                );
            }
        } else {
            result["admitted"] = json!(true);
            run.body = match route.settings.body_policy.inspection {
                crate::body::Inspection::Off => None,
                crate::body::Inspection::Full(limit) => {
                    if input.body.len() > limit {
                        results.push(json!({"status":413,"admitted":false}));
                        continue;
                    }
                    Some(crate::body::BodyView {
                        bytes: input.body.clone().into(),
                        complete: true,
                    })
                }
                crate::body::Inspection::Prefix(limit) => Some(crate::body::BodyView {
                    bytes: bytes::Bytes::copy_from_slice(
                        &input.body.as_bytes()[..input.body.len().min(limit)],
                    ),
                    complete: input.body.len() <= limit,
                }),
            };
            let mut edits = crate::script::Edits::default();
            let mut backend = None;
            let mut forwarded = true;
            if let Some(script) = &route.script {
                match script.request(run.clone()) {
                    Ok((_, outcome)) => {
                        let decision = match outcome.decision {
                            Decision::Pass => json!({"pass":true}),
                            Decision::Proxy(name) => {
                                ensure!(
                                    route.allowed_backends.contains_key(&name),
                                    "plugin selected undeclared backend"
                                );
                                backend = route.allowed_backends.get(&name).cloned();
                                json!({"backend":name})
                            }
                            Decision::Reply(status, body) => {
                                forwarded = false;
                                result["outbound"] =
                                    json!({"kind":"return","status":status,"body":body});
                                json!({"status":status,"body":body})
                            }
                        };
                        result["plugin"] = json!({"decision":decision,"path":outcome.edits.path,"query":outcome.edits.query,"headers":outcome.edits.headers});
                        edits = outcome.edits;
                    }
                    Err(e) => {
                        forwarded = false;
                        result["status"] = json!(500);
                        result["error"] = json!(e.to_string());
                    }
                }
            }
            if forwarded {
                result["outbound"] = super::outbound::Preview {
                    snapshot,
                    listener,
                    route: &route,
                    request: &run,
                    edits: &edits,
                    backend,
                    original_uri: &input.path,
                    original_peer: &input.client,
                    scheme: &scheme,
                    body_len: input.body.len(),
                    sample: sample as u64,
                }
                .finish()?;
            }
            if input.hold_permits {
                permits.extend(held);
            }
        }
        results.push(result);
        request.claims.clear();
    }
    let mut output = if input.repeat == 1 {
        results.remove(0)
    } else {
        json!({"results":results,"repeat":input.repeat,"hold_permits":input.hold_permits})
    };
    output["rate_limit_scope"] = json!("isolated_simulation");
    output["shared_quota_checked"] = json!(false);
    Ok(output)
}
