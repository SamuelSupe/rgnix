use super::*;

pub(super) enum Dispatch {
    Proxy,
    Reply(u16, String, Option<String>),
    Static(Arc<Route>),
}

impl Proxy {
    pub(super) fn dispatch(&self, ctx: &mut Context) -> Result<Dispatch> {
        let route = ctx.route.as_ref().unwrap();
        if let Some(backend) = ctx.backend.take() {
            ctx.backend = Some(
                route
                    .rollout
                    .as_ref()
                    .map_or_else(|| backend.clone(), |r| r.enforce(backend.clone())),
            );
            return Ok(Dispatch::Proxy);
        }
        if let Some(policy) = &route.settings.gateway {
            if let Some((status, location)) =
                policy.location(&ctx.request, &route.matcher, self.tls)
            {
                return Ok(Dispatch::Reply(status, String::new(), Some(location)));
            }
            ctx.backend = Some(
                route
                    .rollout
                    .as_ref()
                    .map_or_else(|| policy.select(), |r| Some(r.select(&ctx.request)))
                    .ok_or_else(|| error(500, "Gateway backend reference is invalid or missing"))?,
            );
            return Ok(Dispatch::Proxy);
        }
        match &route.action {
            Action::Proxy { backend, uri } => {
                ctx.backend = Some(
                    route
                        .rollout
                        .as_ref()
                        .map_or_else(|| backend.clone(), |r| r.select(&ctx.request)),
                );
                ctx.uri = uri.clone();
                Ok(Dispatch::Proxy)
            }
            Action::Return { status, text } => {
                let text = expand(text, ctx, self.tls, "");
                if (300..400).contains(status) && !text.is_empty() {
                    Ok(Dispatch::Reply(*status, String::new(), Some(text)))
                } else {
                    Ok(Dispatch::Reply(*status, text, None))
                }
            }
            Action::Unavailable => Err(error(503, "backend unavailable")),
            Action::Static => Ok(Dispatch::Static(route.clone())),
        }
    }
    pub(super) async fn authorize(
        &self,
        headers: &http::HeaderMap,
        client_certificate: Option<&crate::security::mtls::Peer>,
        ctx: &mut Context,
    ) -> Result<()> {
        let route = ctx.route.as_ref().unwrap().clone();
        if !route.settings.security.mtls.authorize(client_certificate) {
            return Err(error(
                403,
                "client certificate required or no longer trusted",
            ));
        }
        let global_rate = self.shared.controls.active.load().global_rate.clone();
        let rate_group = route
            .tenant
            .as_ref()
            .map_or("standalone", |t| t.name.as_str());
        let rate_capacity = route
            .tenant
            .as_ref()
            .map_or(16384, |t| t.quota.load().max_limiter_keys);
        if let Some(tenant) = &route.tenant {
            ctx.tenant_request = Some(tenant.acquire(crate::tenancy::Resource::Request).map_err(
                |s| {
                    self.shared
                        .telemetry
                        .namespace_rejected(&tenant.name, "request");
                    error(s, "namespace request quota exhausted")
                },
            )?);
            let quota = tenant.quota.load_full();
            let policy = crate::traffic::global::policy(
                global_rate.as_ref(),
                rate_group,
                "_namespace",
                crate::traffic::Policy {
                    rate: Some(crate::traffic::Rate {
                        per_second: quota.requests_per_second,
                        burst: quota.burst,
                        key: crate::traffic::Key::Route,
                    }),
                    concurrency: None,
                },
                &ctx.request,
                rate_capacity,
                &self.shared.telemetry,
            )
            .await
            .map_err(|s| {
                self.shared
                    .telemetry
                    .namespace_rejected(&tenant.name, "rate");
                error(s, "shared namespace rate quota unavailable or exhausted")
            })?;
            if policy.rate.is_some() {
                tenant.rate(&ctx.request).map_err(|s| {
                    self.shared
                        .telemetry
                        .namespace_rejected(&tenant.name, "rate");
                    error(s, "namespace rate quota exhausted")
                })?;
            }
        }
        let traffic = route
            .tenant
            .as_ref()
            .map_or(&self.shared.traffic, |t| &t.traffic);
        let pre_auth_policy = crate::traffic::global::policy(
            global_rate.as_ref(),
            rate_group,
            &route.id,
            route.settings.traffic.phase(true),
            &ctx.request,
            rate_capacity,
            &self.shared.telemetry,
        )
        .await
        .map_err(|s| {
            self.shared
                .telemetry
                .rejected
                .with_label_values(&["rate"])
                .inc();
            error(
                s,
                "shared pre-authentication rate quota unavailable or exhausted",
            )
        })?;
        ctx.pre_auth_permit = traffic
            .acquire(&route.id, &pre_auth_policy, &ctx.request, &ctx.claims)
            .map_err(|status| {
                self.shared
                    .telemetry
                    .rejected
                    .with_label_values(&[if status == 429 {
                        "rate"
                    } else {
                        "route_concurrency"
                    }])
                    .inc();
                error(status, "pre-authentication traffic budget exhausted")
            })?;
        if let Some(jwt) = &route.settings.security.jwt {
            let mut tokens = headers.get_all("authorization").iter();
            let token = tokens
                .next()
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .filter(|_| tokens.next().is_none())
                .ok_or_else(|| error(401, "bearer token required"))?;
            ctx.claims = jwt
                .verify(token)
                .map_err(|_| error(401, "invalid bearer token"))?;
            ctx.request.claims = ctx.claims.clone();
        }
        if let Some(auth) = &route.settings.security.external {
            let _auth_budget = route
                .tenant
                .as_ref()
                .map(|t| t.acquire(crate::tenancy::Resource::Auth))
                .transpose()
                .map_err(|s| {
                    self.shared
                        .telemetry
                        .namespace_rejected(&route.tenant.as_ref().unwrap().name, "auth");
                    error(s, "namespace authentication quota exhausted")
                })?;
            for name in &auth.response_headers {
                ctx.request.headers.remove(name);
                ctx.edits.headers.insert(name.clone(), None);
            }
            let headers = auth
                .authorize(
                    &self.shared.auth_client,
                    &ctx.request,
                    &ctx.original_uri,
                    ctx.trace.as_deref(),
                    self.shared.telemetry.traces.as_ref(),
                )
                .await
                .map_err(|status| error(status, "external authorization rejected"))?;
            for (name, value) in headers {
                ctx.request.headers.insert(name.clone(), value.clone());
                ctx.edits.headers.insert(name, Some(value));
            }
        }
        let post_auth_policy = crate::traffic::global::policy(
            global_rate.as_ref(),
            rate_group,
            &route.id,
            route.settings.traffic.phase(false),
            &ctx.request,
            rate_capacity,
            &self.shared.telemetry,
        )
        .await
        .map_err(|s| {
            self.shared
                .telemetry
                .rejected
                .with_label_values(&["rate"])
                .inc();
            error(s, "shared rate quota unavailable or exhausted")
        })?;
        ctx.traffic_permit = traffic
            .acquire(&route.id, &post_auth_policy, &ctx.request, &ctx.claims)
            .map_err(|status| {
                self.shared
                    .telemetry
                    .rejected
                    .with_label_values(&[if status == 429 {
                        "rate"
                    } else {
                        "route_concurrency"
                    }])
                    .inc();
                error(status, "route traffic budget exhausted")
            })?;
        Ok(())
    }
    pub(super) fn acquire_plugin(&self, ctx: &mut Context) -> Result<()> {
        let route = ctx.route.as_ref().unwrap();
        ctx.tenant_plugin = route
            .tenant
            .as_ref()
            .map(|t| t.acquire(crate::tenancy::Resource::Plugin))
            .transpose()
            .map_err(|s| {
                self.shared
                    .telemetry
                    .namespace_rejected(&route.tenant.as_ref().unwrap().name, "plugin");
                error(s, "namespace plugin quota exhausted")
            })?;
        ctx.plugin_permit = Some(self.shared.plugins.clone().try_acquire_owned().map_err(
            |_| {
                self.shared
                    .telemetry
                    .rejected
                    .with_label_values(&["plugin"])
                    .inc();
                error(503, "plugin instance budget exhausted")
            },
        )?);
        Ok(())
    }
    pub(super) fn observe_inspection(
        &self,
        ctx: &Context,
        started: Instant,
        inspected: &Result<Option<crate::body::BodyView>>,
    ) {
        let route = ctx.route.as_ref().unwrap();
        if let Some(mode) = match route.settings.body_policy.inspection {
            crate::body::Inspection::Off => None,
            crate::body::Inspection::Full(_) => Some("full"),
            crate::body::Inspection::Prefix(_) => Some("prefix"),
        } {
            let result = match inspected {
                Ok(Some(view)) => {
                    self.shared
                        .telemetry
                        .traffic
                        .inspection_bytes
                        .with_label_values(&[mode])
                        .inc_by(view.bytes.len() as u64);
                    if view.complete {
                        "complete"
                    } else {
                        "truncated"
                    }
                }
                Err(e) if e.etype() == &pingora::ErrorType::HTTPStatus(408) => "timeout",
                Err(e) if e.etype() == &pingora::ErrorType::HTTPStatus(413) => "too_large",
                _ => "error",
            };
            self.shared
                .telemetry
                .traffic
                .inspection
                .with_label_values(&[mode, result])
                .observe(started.elapsed().as_secs_f64());
        }
    }
    pub(super) fn request_plugin(
        &self,
        ctx: &mut Context,
        inspected: Option<crate::body::BodyView>,
    ) -> Result<Option<(u16, String)>> {
        let route = ctx.route.as_ref().unwrap().clone();
        let plugin = route.script.as_ref().unwrap();
        let mut plugin_request = std::mem::take(&mut ctx.request);
        plugin_request.body = inspected;
        let plugin_request = Arc::new(plugin_request);
        self.shared.telemetry.plugin_calls.inc();
        let result = {
            let _timer = self.shared.telemetry.plugin_duration.start_timer();
            plugin.request(plugin_request.clone())
        };
        let result = result.map(|(execution, outcome)| {
            if plugin.has_response_hook {
                ctx.execution = Some(execution);
            } else {
                drop(execution);
                ctx.plugin_permit.take();
                ctx.tenant_plugin.take();
            }
            outcome
        });
        // Request-only hooks release their instance before recovering the
        // input, avoiding a deep copy. Response hooks retain an immutable
        // view, and errors still restore the original metadata for logging.
        ctx.request = Arc::unwrap_or_clone(plugin_request);
        ctx.request.body = None;
        match result {
            Ok(outcome) => {
                ctx.edits.path = outcome.edits.path;
                ctx.edits.query = outcome.edits.query;
                ctx.edits.headers.extend(outcome.edits.headers);
                match outcome.decision {
                    script::Decision::Pass => {}
                    script::Decision::Proxy(name) => {
                        let Some(key) = route.allowed_backends.get(&name) else {
                            self.shared.telemetry.plugin_errors.inc();
                            return Err(error(500, "plugin selected an undeclared backend"));
                        };
                        ctx.backend = Some(key.clone());
                    }
                    script::Decision::Reply(status, body) => {
                        return Ok(Some((status, body)));
                    }
                }
            }
            Err(e) => {
                self.shared.telemetry.plugin_errors.inc();
                log::error!("plugin {}: {e:#}", route.id);
                return Err(error(500, "plugin execution failed"));
            }
        }
        Ok(None)
    }
    pub(super) fn select_endpoint(
        &self,
        ctx: &mut Context,
    ) -> Result<Arc<crate::backend::Backend>> {
        let backend = ctx
            .snapshot
            .as_ref()
            .and_then(|s| ctx.backend.as_ref().and_then(|key| s.backends.get(key)))
            .ok_or_else(|| error(503, "backend unavailable"))?;
        let key = match &backend.options.balance {
            crate::backend::Balance::Hash(key) => key.value(&ctx.request, &ctx.claims),
            crate::backend::Balance::Sticky(name) => {
                let existing = ctx.request.headers.get("cookie").and_then(|v| {
                    v.split(';')
                        .filter_map(|v| v.trim().split_once('='))
                        .find(|(k, v)| {
                            k == name && v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                        .map(|(_, v)| v.to_owned())
                });
                existing.unwrap_or_else(|| {
                    let value = format!("{:032x}", rand::random::<u128>());
                    ctx.affinity_cookie = Some(format!(
                        "{name}={value}; Path=/; Max-Age=86400; HttpOnly; SameSite=Lax{}",
                        if ctx.scheme == "https" {
                            "; Secure"
                        } else {
                            ""
                        }
                    ));
                    value
                })
            }
            _ => String::new(),
        };
        let lease = backend
            .select(&key)
            .ok_or_else(|| error(503, "no ready endpoint or backend budget exhausted"))?;
        let address = lease.address;
        if let Some(trace) = &mut ctx.trace {
            trace.upstream = Some(trace.client(
                &ctx.request.method,
                ctx.backend.as_deref().unwrap_or("-"),
                "proxy",
            ));
        }
        ctx.upstream_lease = Some(lease);
        ctx.upstream_address = Some(address);
        ctx.upstream_started = Some(Instant::now());
        ctx.upstream_label = ctx
            .backend
            .as_ref()
            .map(|b| self.shared.telemetry.label("backend", b).to_owned());
        Ok(backend.clone())
    }
    pub(super) fn upstream_headers(
        &self,
        request: &mut RequestHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        let snapshot = ctx.snapshot.as_ref().unwrap();
        let backend = &snapshot.backends[ctx.backend.as_ref().unwrap()];
        let route = ctx.route.as_ref().unwrap();
        let mut edits = ctx.edits.clone();
        if edits.path.is_none()
            && let Some(rewrite) = route
                .settings
                .gateway
                .as_ref()
                .and_then(|p| p.rewrite.as_ref())
            && let Some(path) = &rewrite.path
        {
            edits.path = Some(path.apply(&ctx.request.path, &route.matcher));
        }
        if edits.path.is_some()
            || edits.query.is_some()
            || ctx.uri.is_some()
            || request.uri.scheme().is_some()
            || request.uri.authority().is_some()
        {
            let target = planning::outbound_uri(
                &ctx.original_uri,
                &ctx.request,
                &edits,
                &route.matcher,
                ctx.uri.as_deref(),
            );
            let target: http::Uri = target
                .parse()
                .map_err(|_| error(500, "invalid rewritten URI"))?;
            request.set_uri(target);
        }
        strip_hop_headers(request);
        if ctx.request.headers.get("te").is_some_and(|v| {
            v.split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("trailers"))
        }) {
            request.insert_header("TE", "trailers")?;
        }
        if let Some(policy) = &route.settings.gateway {
            if let Some(host) = policy.rewrite.as_ref().and_then(|r| r.hostname.as_deref()) {
                request.insert_header("Host", host)?;
            } else if let Some(host) = ctx.request.headers.get("host") {
                request.insert_header("Host", host)?;
            } else {
                request.insert_header("Host", ctx.request.host.as_str())?;
            }
            policy.request_headers.request(request)?;
        } else {
            request.insert_header("Host", backend.host_header.as_str())?;
        }
        for (name, value) in &route.settings.request_headers {
            let value = expand(value, ctx, self.tls, &backend.host_header);
            if value.is_empty() {
                request.remove_header(name);
            } else {
                request.insert_header(name.clone(), value)?;
            }
        }
        for (name, value) in &ctx.edits.headers {
            if let Some(value) = value {
                request.insert_header(name.clone(), value.as_str())?;
            } else {
                request.remove_header(name);
            }
        }
        if let Some(trace) = &ctx.trace
            && trace.enabled
            && let Some(span) = &trace.upstream
        {
            let mut headers = http::HeaderMap::new();
            span.inject(&mut headers);
            request.remove_header("tracestate");
            for (name, value) in &headers {
                request.insert_header(name.clone(), value.clone())?;
            }
        }
        // Upgrade headers are transport state and cannot be manufactured by a script.
        if ctx
            .request
            .headers
            .get("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            request.insert_header("Upgrade", "websocket")?;
            request.insert_header("Connection", "upgrade")?;
        }
        Ok(())
    }
}
