pub mod bundle;
mod policy;
mod security;
mod syntax;
mod upstream;
use crate::{model::*, script::Compiler};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use syntax::Directive;

#[derive(Clone, Default)]
struct Scope {
    settings: Settings,
    action: Option<Action>,
    script: Option<PathBuf>,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    upstream_cert: Option<PathBuf>,
    upstream_key: Option<PathBuf>,
    http2: bool,
    error_log: Option<Arc<crate::logging::ErrorOutput>>,
    log_rotation: crate::logging::Rotation,
}

pub fn load(path: &Path, compiler: &Compiler, version: u64) -> Result<RuntimeSnapshot> {
    load_direct(path, compiler, version)
}
fn load_direct(path: &Path, compiler: &Compiler, version: u64) -> Result<RuntimeSnapshot> {
    let absolute = path.canonicalize()?;
    let base = absolute.parent().unwrap();
    let nodes = syntax::load(&absolute)?;
    load_nodes(&nodes, base, compiler, version)
}
fn load_nodes(
    nodes: &[Directive],
    base: &Path,
    compiler: &Compiler,
    version: u64,
) -> Result<RuntimeSnapshot> {
    let mut http = None;
    for node in nodes {
        match node.name.as_str() {
            "http" => {
                ensure!(http.is_none(), "{}: duplicate http", node.source);
                ensure!(
                    node.args.is_empty(),
                    "{}: http takes no arguments",
                    node.source
                );
                http = Some(block(node)?);
            }
            "events" => ensure!(
                node.args.is_empty() && block(node)?.is_empty(),
                "{}: only empty events is supported",
                node.source
            ),
            _ => bail!(
                "{}: unsupported top-level directive {}",
                node.source,
                node.name
            ),
        }
    }
    let http = http.context("configuration requires an http block")?;
    let mut result = RuntimeSnapshot::empty(vec![]);
    result.version = version;
    let mut scope = Scope::default();
    scope.settings.root = base.join("html");
    for n in http {
        if n.name != "server" && n.name != "upstream" {
            apply(&mut scope, n, base, "http")?;
        }
    }
    result.error_log = scope.error_log.clone();
    result.log_rotation = scope.log_rotation.clone();
    result.default_access_log = scope.settings.access_log.clone();
    for n in http.iter().filter(|n| n.name == "upstream") {
        let backend = upstream::load(n).with_context(|| n.source.clone())?;
        ensure!(
            !result.backends.contains_key(&n.args[0]),
            "{}: duplicate upstream",
            n.source
        );
        result.backends.insert(n.args[0].clone(), Arc::new(backend));
    }
    let mut listener_map: BTreeMap<SocketAddr, Listener> = BTreeMap::new();
    let mut defaults = BTreeSet::new();
    for (server_index, server) in http.iter().filter(|n| n.name == "server").enumerate() {
        ensure!(
            server.args.is_empty(),
            "{}: server block takes no arguments",
            server.source
        );
        let children = block(server)?;
        let mut local = scope.clone();
        reset_header_inheritance(&mut local, children);
        let mut names = vec![];
        let mut listens = vec![];
        for n in children {
            match n.name.as_str() {
                "server_name" => {
                    leaf(n)?;
                    ensure!(!n.args.is_empty(), "{}: expected server names", n.source);
                    for name in &n.args {
                        validate_host(name).with_context(|| n.source.clone())?;
                        names.push(name.to_ascii_lowercase());
                    }
                }
                "listen" => listens.push(parse_listen(n)?),
                "location" => {}
                _ => apply(&mut local, n, base, "server")?,
            }
        }
        if names.is_empty() {
            names.push(String::new());
        }
        if listens.is_empty() {
            listens.push(("0.0.0.0:80".parse()?, false, false, false));
        }
        let mut routes = vec![];
        for n in children.iter().filter(|n| n.name == "location") {
            let matcher = match n.args.as_slice() {
                [p] if p.starts_with('/') => PathMatch::NginxPrefix(p.clone()),
                [eq, p] if eq == "=" && p.starts_with('/') => PathMatch::Exact(p.clone()),
                _ => bail!(
                    "{}: only plain-prefix and exact locations are supported",
                    n.source
                ),
            };
            ensure!(
                !routes.iter().any(|r: &Arc<Route>| r.matcher == matcher),
                "{}: duplicate location",
                n.source
            );
            let mut location = local.clone();
            reset_header_inheritance(&mut location, block(n)?);
            for d in block(n)? {
                apply(&mut location, d, base, "location")?;
            }
            routes.push(
                build_route(
                    format!("server-{server_index}:{}", matcher.path()),
                    matcher,
                    location,
                    compiler,
                    &mut result,
                )
                .with_context(|| n.source.clone())?,
            );
        }
        if !routes
            .iter()
            .any(|r| r.matcher == PathMatch::NginxPrefix("/".into()))
        {
            routes.push(
                build_route(
                    format!("server-{server_index}:/"),
                    PathMatch::NginxPrefix("/".into()),
                    local.clone(),
                    compiler,
                    &mut result,
                )
                .with_context(|| server.source.clone())?,
            );
        }
        if matches!(local.action, Some(Action::Return { .. })) {
            // NGINX executes server-level return before selecting a location.
            let mut early = local.clone();
            early.script = None;
            early.settings.max_body = 0;
            routes = vec![build_route(
                format!("server-{server_index}:return"),
                PathMatch::NginxPrefix("/".into()),
                early,
                compiler,
                &mut result,
            )?];
        }
        let certificate = match (&local.cert, &local.key) {
            (Some(cert), Some(key)) => Some(Arc::new(Certificate::parse(
                &std::fs::read(cert)?,
                &std::fs::read(key)?,
            )?)),
            (None, None) => None,
            _ => bail!(
                "{}: both ssl_certificate and ssl_certificate_key are required",
                server.source
            ),
        };
        for (address, tls, explicit_default, proxy_protocol) in listens {
            if explicit_default {
                ensure!(
                    defaults.insert(address),
                    "{}: duplicate default_server",
                    server.source
                );
            }
            let listener = Listener {
                address,
                tls,
                http2: local.http2,
                proxy_protocol,
                proxy_trusted: if proxy_protocol {
                    local.settings.identity.trusted.clone()
                } else {
                    vec![]
                },
            };
            ensure!(
                !proxy_protocol || !listener.proxy_trusted.is_empty(),
                "{}: PROXY protocol requires set_real_ip_from",
                server.source
            );
            if let Some(existing) = listener_map.get(&address) {
                ensure!(
                    existing == &listener,
                    "{}: conflicting TLS/http2 options on listener",
                    server.source
                );
            }
            listener_map.insert(address, listener);
            let first = !result.hosts.iter().any(|h| h.listener == address);
            if explicit_default {
                for host in &mut result.hosts {
                    if host.listener == address {
                        host.default = false;
                    }
                }
                for cert in &mut result.certificates {
                    if cert.listener == address {
                        cert.default = false;
                    }
                }
            }
            let default = explicit_default || first;
            if tls {
                let certificate = certificate
                    .as_ref()
                    .context("TLS listener requires a certificate")?;
                for name in &names {
                    certificate.validate_name(name)?;
                    result.certificates.push(TlsHost {
                        listener: address,
                        name: name.clone(),
                        ingress: false,
                        default,
                        certificate: Some(certificate.clone()),
                        client_auth: local.settings.security.mtls.clone(),
                    });
                }
            }
            result.hosts.push(VirtualHost {
                listener: address,
                names: names.clone(),
                default,
                ingress: false,
                routes: routes.clone(),
            });
        }
    }
    ensure!(
        !result.hosts.is_empty(),
        "http requires at least one server"
    );
    let has_scripts = result
        .hosts
        .iter()
        .flat_map(|host| &host.routes)
        .any(|route| route.script.is_some());
    // Bind aliases after every location is loaded, including HTTPS uses declared later.
    for upstream in http.iter().filter(|n| n.name == "upstream") {
        let name = &upstream.args[0];
        let transports: BTreeMap<_, _> = result
            .backends
            .iter()
            .filter(|(key, backend)| key.contains("://") && &backend.hostname == name)
            .map(|(_, backend)| (backend.tls, backend.clone()))
            .collect();
        ensure!(
            !has_scripts || transports.len() <= 1,
            "{}: upstream {name} uses both HTTP and HTTPS; its script backend alias is ambiguous",
            upstream.source
        );
        if transports.len() == 1 {
            result
                .backends
                .insert(name.clone(), transports.into_values().next().unwrap());
        }
    }
    // Health and data connections must use the same identity. A named group can
    // be used with different CA/SNI/client certificates in different locations.
    let base_backends = result.backends.clone();
    for host in &mut result.hosts {
        for route in &mut host.routes {
            let route = Arc::make_mut(route);
            for key in route.allowed_backends.values_mut() {
                if let Some(backend) = base_backends.get(key)
                    && backend.options.health.is_some()
                {
                    let profile = &route.settings.upstream;
                    let configured_key = format!("{key}#tls-{:016x}", profile.pool_key());
                    result
                        .backends
                        .entry(configured_key.clone())
                        .or_insert_with(|| {
                            let mut configured = backend.transport(
                                backend.tls,
                                backend.hostname.clone(),
                                backend.host_header.clone(),
                            );
                            configured.profile = profile.clone();
                            Arc::new(configured)
                        });
                    if let Action::Proxy { backend, .. } = &mut route.action
                        && backend == key
                    {
                        *backend = configured_key.clone();
                    }
                    *key = configured_key;
                }
            }
        }
    }
    result.listeners = listener_map.into_values().collect();
    result.content_hash = result.fingerprint()?;
    result.reindex();
    Ok(result)
}

fn build_route(
    id: String,
    matcher: PathMatch,
    mut scope: Scope,
    compiler: &Compiler,
    snapshot: &mut RuntimeSnapshot,
) -> Result<Arc<Route>> {
    ensure!(
        scope.settings.security.mtls.mode == crate::security::mtls::Mode::Off
            || scope.settings.security.mtls.store.is_some(),
        "ssl_verify_client requires ssl_client_certificate"
    );
    match (&scope.upstream_cert, &scope.upstream_key) {
        (Some(cert), Some(key)) => scope.settings.upstream.set_identity(
            std::fs::read_to_string(cert)?,
            std::fs::read_to_string(key)?,
        )?,
        (None, None) => {}
        _ => {
            bail!("proxy_ssl_certificate and proxy_ssl_certificate_key must be configured together")
        }
    }
    let mut action = scope.action.unwrap_or(Action::Static);
    if let Action::Proxy { backend, uri } = &mut action {
        let parsed = proxy_url(backend)?;
        let host = match parsed.host().context("proxy_pass missing host")? {
            url::Host::Domain(name) => name.to_owned(),
            url::Host::Ipv4(address) => address.to_string(),
            url::Host::Ipv6(address) => address.to_string(),
        };
        let authority = &backend[backend.find("://").unwrap() + 3..];
        *uri = authority.find('/').map(|i| authority[i..].to_string());
        if let Some(query) = authority.find('?') {
            ensure!(
                authority.find('/').is_some_and(|slash| slash < query),
                "proxy_pass query requires a URI path"
            );
        }
        let key = backend.split('/').take(3).collect::<Vec<_>>().join("/");
        if let Some(group) = snapshot.backends.get(&host).cloned() {
            ensure!(
                parsed.port().is_none(),
                "named upstream cannot have an explicit port"
            );
            snapshot.backends.entry(key.clone()).or_insert_with(|| {
                Arc::new(group.transport(parsed.scheme() == "https", host.clone(), host.clone()))
            });
        } else if !snapshot.backends.contains_key(&key) {
            let port = parsed
                .port_or_known_default()
                .context("missing upstream port")?;
            let addresses: Vec<_> = (host.as_str(), port)
                .to_socket_addrs()?
                .map(|address| Endpoint { address, weight: 1 })
                .collect();
            ensure!(!addresses.is_empty(), "upstream has no resolved addresses");
            let host_header =
                parsed[url::Position::BeforeHost..url::Position::AfterPort].to_string();
            let mut resolved = Backend::new(
                addresses,
                parsed.scheme() == "https",
                host.clone(),
                host_header,
            );
            resolved.origins.push(crate::backend::Origin {
                host,
                port,
                weight: 1,
            });
            snapshot.backends.insert(key.clone(), Arc::new(resolved));
        }
        *backend = key;
    }
    let script = scope
        .script
        .as_ref()
        .map(|p| compiler.load(p))
        .transpose()?;
    let allowed_backends = snapshot
        .backends
        .keys()
        .map(|key| (key.clone(), key.clone()))
        .collect();
    Ok(Arc::new(Route {
        id,
        tenant: None,
        rollout: None,
        matcher,
        action,
        settings: scope.settings,
        script,
        allowed_backends,
    }))
}

fn proxy_url(value: &str) -> Result<url::Url> {
    ensure!(
        value.starts_with("http://") || value.starts_with("https://"),
        "proxy_pass requires an absolute http/https URL"
    );
    let parsed = url::Url::parse(value)?;
    ensure!(
        parsed.query().is_none(),
        "proxy_pass URLs with query strings are unsupported; use req.set_query instead"
    );
    ensure!(
        parsed.host().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.fragment().is_none(),
        "proxy_pass requires a host without credentials or fragment"
    );
    Ok(parsed)
}

fn reset_header_inheritance(scope: &mut Scope, nodes: &[Directive]) {
    if nodes.iter().any(|n| n.name == "set_real_ip_from") {
        scope.settings.identity.trusted.clear();
    }
    if nodes.iter().any(|n| n.name == "allow" || n.name == "deny") {
        scope.settings.identity.access.clear();
    }
    if nodes.iter().any(|n| n.name == "proxy_set_header") {
        scope.settings.request_headers.clear();
    }
    if nodes.iter().any(|n| n.name == "add_header") {
        scope.settings.response_headers.clear();
    }
}

fn apply(s: &mut Scope, n: &Directive, base: &Path, context: &str) -> Result<()> {
    let result = (|| -> Result<()> {
        if n.name == "types" {
            ensure!(n.args.is_empty(), "types takes no arguments");
            s.settings.mime.clear();
            for mapping in block(n)? {
                leaf(mapping)?;
                ensure!(!mapping.args.is_empty(), "MIME type requires extensions");
                for ext in &mapping.args {
                    s.settings
                        .mime
                        .insert(ext.to_ascii_lowercase(), mapping.name.clone());
                }
            }
            return Ok(());
        }
        leaf(n)?;
        if policy::apply(s, n)? || security::apply(s, n, base, context)? {
            return Ok(());
        }
        match n.name.as_str() {
            "root" => {
                one(n)?;
                ensure!(!n.args[0].contains('$'), "root variables are unsupported");
                s.settings.root = base.join(&n.args[0]);
                s.settings.alias = None;
            }
            "alias" => {
                one(n)?;
                ensure!(
                    context == "location" && !n.args[0].contains('$'),
                    "alias requires a literal directory in location"
                );
                s.settings.alias = Some(base.join(&n.args[0]));
            }
            "try_files" => {
                ensure!(
                    (2..=8).contains(&n.args.len()),
                    "try_files expects 2..8 candidates with an absolute fallback URI or =status"
                );
                for (i, v) in n.args.iter().enumerate() {
                    if i + 1 == n.args.len() && v.starts_with('=') {
                        let code: u16 = v[1..].parse()?;
                        ensure!(
                            (400..=599).contains(&code),
                            "try_files fallback status must be 400..599"
                        );
                    } else {
                        ensure!(
                            (v == "$uri"
                                || v == "$uri/"
                                || (v.starts_with('/') && !v.contains(['$', '?', '#']))),
                            "try_files supports $uri, $uri/ and literal absolute paths"
                        );
                    }
                }
                let last = n.args.last().unwrap();
                ensure!(
                    last.starts_with('/') || last.starts_with('='),
                    "final try_files candidate must be a literal path or =status"
                );
                s.settings.try_files = n.args.clone();
            }
            "index" => {
                ensure!(
                    !n.args.is_empty() && n.args.iter().all(|x| !x.contains('/') && x != ".."),
                    "index requires file names"
                );
                s.settings.index = n.args.clone();
            }
            "default_type" => {
                one(n)?;
                http::HeaderValue::from_str(&n.args[0])?;
                s.settings.default_type = n.args[0].clone();
            }
            "proxy_http_version" => {
                one(n)?;
                s.settings.upstream.protocol = crate::upstream::Protocol::parse(&n.args[0])?;
            }
            "proxy_ssl_certificate" | "proxy_ssl_certificate_key" => {
                one(n)?;
                if n.name == "proxy_ssl_certificate" {
                    s.upstream_cert = Some(base.join(&n.args[0]));
                } else {
                    s.upstream_key = Some(base.join(&n.args[0]));
                }
            }
            "proxy_ssl_name" => {
                one(n)?;
                ensure!(
                    !n.args[0].contains('$') && n.args[0].len() <= 253,
                    "proxy_ssl_name requires a literal host"
                );
                s.settings.upstream.server_name = Some(n.args[0].clone());
            }
            "proxy_ssl_trusted_certificate" => {
                one(n)?;
                s.settings
                    .upstream
                    .set_ca(std::fs::read_to_string(base.join(&n.args[0]))?)?;
            }
            "proxy_ssl_verify" | "proxy_ssl_server_name" => {
                one(n)?;
                ensure!(
                    n.args[0] == "on",
                    "upstream TLS always verifies certificates and hostnames"
                );
            }
            "proxy_pass" => {
                one(n)?;
                ensure!(
                    context == "location",
                    "proxy_pass is only valid in location"
                );
                ensure!(
                    !n.args[0].contains('$'),
                    "proxy_pass variables are unsupported"
                );
                proxy_url(&n.args[0])?;
                if !matches!(s.action, Some(Action::Return { .. })) {
                    s.action = Some(Action::Proxy {
                        backend: n.args[0].clone(),
                        uri: None,
                    });
                }
            }
            "return" => {
                ensure!(
                    context != "http" && (1..=2).contains(&n.args.len()),
                    "return expects status [text] in server or location"
                );
                let status = n.args[0].parse()?;
                ensure!(
                    (200..=599).contains(&status),
                    "return supports status 200..599"
                );
                let text = n.args.get(1).cloned().unwrap_or_default();
                validate_variables(&text)?;
                if !matches!(s.action, Some(Action::Return { .. })) {
                    s.action = Some(Action::Return { status, text });
                }
            }
            "proxy_set_header" => {
                ensure!(n.args.len() == 2, "proxy_set_header expects name value");
                http::header::HeaderName::from_bytes(n.args[0].as_bytes())?;
                validate_variables(&n.args[1])?;
                s.settings
                    .request_headers
                    .push((n.args[0].clone(), n.args[1].clone()));
            }
            "add_header" => {
                ensure!(
                    (2..=3).contains(&n.args.len()) && n.args.get(2).is_none_or(|x| x == "always"),
                    "add_header expects name value [always]"
                );
                http::header::HeaderName::from_bytes(n.args[0].as_bytes())?;
                validate_variables(&n.args[1])?;
                s.settings.response_headers.push(AddedHeader {
                    name: n.args[0].clone(),
                    value: n.args[1].clone(),
                    always: n.args.len() == 3,
                });
            }
            "client_max_body_size" => {
                one(n)?;
                s.settings.max_body = size(&n.args[0])?;
            }
            "rgnix_request_body" => {
                s.settings.body_policy.inspection =
                    crate::body::Inspection::parse(&n.args.join(" "))?;
            }
            "rgnix_request_body_timeout" => {
                one(n)?;
                s.settings.body_policy.timeout =
                    crate::body::BodyPolicy::parse_timeout(&n.args[0])?;
            }
            "proxy_connect_timeout" => {
                one(n)?;
                s.settings.connect_timeout = duration(&n.args[0])?;
            }
            "proxy_read_timeout" => {
                one(n)?;
                s.settings.read_timeout = duration(&n.args[0])?;
            }
            "proxy_send_timeout" => {
                one(n)?;
                s.settings.write_timeout = duration(&n.args[0])?;
            }
            "keepalive_timeout" => {
                one(n)?;
                s.settings.keepalive = duration(&n.args[0])?;
            }
            "ssl_certificate" => {
                one(n)?;
                ensure!(context != "location", "TLS is not valid in location");
                s.cert = Some(base.join(&n.args[0]));
            }
            "ssl_certificate_key" => {
                one(n)?;
                ensure!(context != "location", "TLS is not valid in location");
                s.key = Some(base.join(&n.args[0]));
            }
            "http2" => {
                one(n)?;
                ensure!(context != "location", "http2 is not valid in location");
                s.http2 = match n.args[0].as_str() {
                    "on" => true,
                    "off" => false,
                    _ => bail!("http2 expects on/off"),
                };
            }
            "rgnix_script" => {
                one(n)?;
                ensure!(
                    context != "http",
                    "rgnix_script belongs to server or location"
                );
                s.script = Some(base.join(&n.args[0]));
            }
            "access_log" => {
                ensure!(
                    (1..=2).contains(&n.args.len())
                        && n.args.get(1).is_none_or(|v| v == "combined" || v == "json"),
                    "access_log expects path [combined|json] or off"
                );
                s.settings.log_policy.json = n.args.get(1).is_some_and(|v| v == "json");
                s.settings.access_log = if n.args[0] == "off" {
                    None
                } else {
                    Some(base.join(&n.args[0]))
                };
            }
            "rgnix_log_rotation" => {
                ensure!(context == "http", "rgnix_log_rotation belongs in http");
                s.log_rotation = crate::logging::Rotation::parse(&n.args)?;
            }
            "error_log" => {
                ensure!(
                    context == "http" && (1..=2).contains(&n.args.len()),
                    "error_log expects path [level] in http"
                );
                let level = n.args.get(1).map(String::as_str).unwrap_or("error");
                ensure!(
                    ["error", "warn", "info", "debug"].contains(&level),
                    "error_log level must be error/warn/info/debug"
                );
                let path = if n.args[0] == "stderr" {
                    PathBuf::from("stderr")
                } else {
                    base.join(&n.args[0])
                };
                s.error_log = Some(crate::logging::ErrorOutput::open(&path, level)?);
            }
            _ => bail!("unsupported directive {} in {context}", n.name),
        }
        Ok(())
    })();
    result.with_context(|| n.source.clone())
}

pub fn validate_variables(value: &str) -> Result<()> {
    let mut rest = value;
    while let Some(i) = rest.find('$') {
        rest = &rest[i + 1..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let variable = &rest[..end];
        ensure!(
            matches!(
                variable,
                "host"
                    | "http_host"
                    | "scheme"
                    | "request_uri"
                    | "uri"
                    | "args"
                    | "request_method"
                    | "remote_addr"
                    | "realip_remote_addr"
                    | "proxy_host"
                    | "proxy_add_x_forwarded_for"
            ) || variable.starts_with("http_") && variable.len() > 5,
            "unsupported variable ${variable}"
        );
        rest = &rest[end..];
    }
    Ok(())
}

pub fn validate_host(host: &str) -> Result<()> {
    let plain = host.strip_prefix("*.").unwrap_or(host);
    ensure!(
        !plain.is_empty()
            && plain
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._".contains(&c)),
        "unsupported server_name {host}"
    );
    Ok(())
}
fn block(n: &Directive) -> Result<&[Directive]> {
    n.children
        .as_deref()
        .with_context(|| format!("{}: {} requires a block", n.source, n.name))
}
fn leaf(n: &Directive) -> Result<()> {
    ensure!(
        n.children.is_none(),
        "{}: {} does not take a block",
        n.source,
        n.name
    );
    Ok(())
}
fn one(n: &Directive) -> Result<()> {
    ensure!(
        n.args.len() == 1,
        "{}: {} expects one argument",
        n.source,
        n.name
    );
    Ok(())
}
fn parse_listen(n: &Directive) -> Result<(SocketAddr, bool, bool, bool)> {
    leaf(n)?;
    ensure!(!n.args.is_empty(), "{}: missing listen address", n.source);
    let address = if let Ok(port) = n.args[0].parse::<u16>() {
        SocketAddr::from(([0, 0, 0, 0], port))
    } else {
        n.args[0].parse().with_context(|| n.source.clone())?
    };
    for option in &n.args[1..] {
        ensure!(
            matches!(option.as_str(), "ssl" | "default_server" | "proxy_protocol"),
            "{}: unsupported listen option {option}",
            n.source
        );
    }
    Ok((
        address,
        n.args.iter().any(|s| s == "ssl"),
        n.args.iter().any(|s| s == "default_server"),
        n.args.iter().any(|s| s == "proxy_protocol"),
    ))
}
pub(crate) fn size(s: &str) -> Result<u64> {
    let (n, multiplier) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1024),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    n.parse::<u64>()?
        .checked_mul(multiplier)
        .context("size overflow")
}
pub(crate) fn duration(s: &str) -> Result<Duration> {
    let (n, multiplier) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60000)
    } else {
        (s, 1000)
    };
    Ok(Duration::from_millis(
        n.parse::<u64>()?
            .checked_mul(multiplier)
            .context("duration overflow")?,
    ))
}
