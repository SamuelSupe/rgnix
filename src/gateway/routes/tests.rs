use super::*;
use crate::model::{Action, Settings};
use serde_json::json;

#[test]
fn indexed_routes_preserve_linear_matching_and_priority() {
    let address = "127.0.0.1:8080".parse().unwrap();
    let mut entries = Vec::new();
    for host in ["", "*.test", "*.example.test", "api.example.test"] {
        for path in ["/", "/api", "/api/", "/api////", "/api/v1", "/中文"] {
            for kind in ["Exact", "PathPrefix"] {
                for predicates in [
                    json!({}),
                    json!({"method":"POST"}),
                    json!({"headers":[{"name":"X-Canary","value":"1"}]}),
                    json!({"queryParams":[{"name":"v","value":"a b"}]}),
                ] {
                    let mut matcher = predicates;
                    matcher["path"] = json!({"type":kind,"value":path});
                    let id = format!("route-{}", entries.len());
                    let mut candidate = entry(&id, host, matcher);
                    candidate.order.0 = format!("2026-01-0{}", entries.len() % 3 + 1);
                    entries.push(candidate);
                }
            }
        }
    }
    for method in [
        json!({}),
        json!({"service":"qa.Echo"}),
        json!({"service":"qa.Echo","method":"Unary"}),
    ] {
        let mut candidate = entry(&format!("grpc-{}", entries.len()), "", json!({}));
        candidate.grpc = true;
        candidate.matcher.method = Some(method);
        entries.push(candidate);
    }
    let mut duplicate = entries[0].clone();
    duplicate.route = Arc::new(Route {
        id: "last-equal-rank".into(),
        ..(*duplicate.route).clone()
    });
    entries.push(duplicate);
    let mut routing = Routing::default();
    for host in ["", "*.test", "*.example.test", "api.example.test"] {
        routing.listeners.push(ListenerRoutes {
            address,
            hostname: host.into(),
            entries: if host == "api.example.test" {
                vec![entry("shadowed-listener", "", json!({}))]
            } else {
                entries.clone()
            },
        });
    }
    routing.listeners.push(ListenerRoutes {
        address,
        hostname: "empty.example.test".into(),
        entries: vec![],
    });
    // Equal listener hostnames select the last listener, not a merged rule set.
    routing.listeners.push(ListenerRoutes {
        address,
        hostname: "api.example.test".into(),
        entries,
    });
    routing.reindex();
    for host in [
        "unmatched",
        "example.test",
        "a.b.example.test",
        "API.EXAMPLE.TEST",
        "empty.example.test",
    ] {
        for path in [
            "",
            "relative",
            "/",
            "/api",
            "/apix",
            "/api/",
            "/api/v1/item",
            "/api////x",
            "/中文/值",
            "/qa.Echo/Unary",
            "/qa.Echo/Stream",
        ] {
            for method in ["GET", "POST"] {
                for query in ["", "v=a+b&v=no", "v=no&v=a+b", "%76=a%20b", "v=%FF"] {
                    for headers in [
                        vec![],
                        vec![("x-canary", "1")],
                        vec![("content-type", "application/grpc+proto;v=1")],
                    ] {
                        let request = RequestData {
                            host: host.into(),
                            path: path.into(),
                            method: method.into(),
                            query: query.into(),
                            headers: headers
                                .into_iter()
                                .map(|(k, v)| (k.into(), v.into()))
                                .collect(),
                            ..Default::default()
                        };
                        let expected = linear_route(&routing, address, &request);
                        let actual = routing.route(address, &request);
                        assert_eq!(
                            actual.as_ref().map(|r| &r.id),
                            expected.as_ref().map(|r| &r.id),
                            "{host} {method} {path}?{query}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
#[ignore = "release-mode routing microbenchmark; run alone on an idle pinned CPU"]
fn routing_scale_benchmark() {
    use std::{
        hint::black_box,
        time::{Duration, Instant},
    };
    type Resolver = fn(&Routing, SocketAddr, &RequestData) -> Option<Arc<Route>>;
    fn measure(
        resolve: Resolver,
        routing: &Routing,
        address: SocketAddr,
        requests: &[RequestData],
    ) -> (u64, u128) {
        let start = Instant::now();
        let mut calls = 0;
        while start.elapsed() < Duration::from_millis(500) {
            for _ in 0..16 {
                black_box(resolve(
                    black_box(routing),
                    address,
                    black_box(&requests[calls as usize % requests.len()]),
                ));
                calls += 1;
            }
        }
        (calls, start.elapsed().as_nanos())
    }
    let address = "127.0.0.1:8080".parse().unwrap();
    let mut results = vec![];
    for shape in [
        "prefix",
        "host",
        "query",
        "prefix-no-query",
        "host-no-query",
        "aa-indexed-prefix",
    ] {
        for count in [1, 100, 1_000, 10_000] {
            if shape.starts_with("aa-") && count != 1_000 {
                continue;
            }
            let mut entries = vec![];
            for i in 0..count {
                let path = if shape.contains("prefix") {
                    format!("/api/{i}")
                } else {
                    "/api".into()
                };
                let host = if shape.starts_with("host") {
                    format!("{i}.example.test")
                } else {
                    "example.test".into()
                };
                let mut matcher = json!({"path":{"value":path}});
                if shape == "query" {
                    matcher["queryParams"] = json!([{"name":"route","value":i.to_string()}]);
                }
                entries.push(entry(&format!("r{i:05}"), &host, matcher));
            }
            let mut routing = Routing::default();
            routing.listeners.push(ListenerRoutes {
                address,
                hostname: String::new(),
                entries,
            });
            let start = Instant::now();
            routing.reindex();
            let build_ns = start.elapsed().as_nanos();
            let requests: Vec<_> = (0..8)
                .map(|slot| {
                    let i = slot * (count - 1) / 7;
                    RequestData {
                        host: if shape.starts_with("host") {
                            format!("{i}.example.test")
                        } else {
                            "example.test".into()
                        },
                        path: if shape.contains("prefix") {
                            format!("/api/{i}/item")
                        } else {
                            "/api/item".into()
                        },
                        method: "GET".into(),
                        query: if shape.ends_with("no-query") {
                            String::new()
                        } else {
                            format!("ignored=a%20b&route={i}&route=duplicate")
                        },
                        ..Default::default()
                    }
                })
                .collect();
            for request in &requests {
                assert_eq!(
                    routing.route(address, request).unwrap().id,
                    linear_route(&routing, address, request).unwrap().id
                );
            }
            let resolvers: [(&str, Resolver); 2] = if shape.starts_with("aa-") {
                [("indexed-a", Routing::route), ("indexed-b", Routing::route)]
            } else {
                [("linear", linear_route), ("indexed", Routing::route)]
            };
            for (_, resolve) in resolvers {
                measure(resolve, &routing, address, &requests);
            }
            let mut windows = vec![];
            for round in 0..3 {
                let order = if round % 2 == 0 { [0, 1] } else { [1, 0] };
                for which in order {
                    let (engine, resolve) = resolvers[which];
                    let (calls, elapsed_ns) = measure(resolve, &routing, address, &requests);
                    windows.push(json!({"round":round+1,"engine":engine,"calls":calls,"elapsed_ns":elapsed_ns,"ns_per_lookup":elapsed_ns as f64 / calls as f64}));
                }
            }
            results.push(
                json!({"shape":shape,"routes":count,"index_build_ns":build_ns,"windows":windows}),
            );
        }
    }
    let output =
        serde_json::to_string_pretty(&json!({"kind":"routing-microbenchmark","results":results}))
            .unwrap();
    if let Ok(path) = std::env::var("RGNIX_ROUTING_BENCH_OUTPUT") {
        std::fs::write(path, output).unwrap();
    } else {
        println!("{output}");
    }
}

#[test]
fn scoped_views_rebuild_gateway_index_without_mutating_published_routes() {
    use crate::{
        diagnostics::auth::{Principal, Role},
        model::RuntimeSnapshot,
    };
    let address = "127.0.0.1:8080".parse().unwrap();
    let mut routing = Routing::default();
    routing.listeners.push(ListenerRoutes {
        address,
        hostname: String::new(),
        entries: vec![
            entry("alpha/app", "alpha.test", json!({})),
            entry("beta/app", "beta.test", json!({})),
        ],
    });
    let mut snapshot = RuntimeSnapshot::empty(vec![]);
    snapshot.gateway = Some(routing);
    let fingerprint = snapshot.fingerprint().unwrap();
    snapshot.reindex();
    assert_eq!(snapshot.fingerprint().unwrap(), fingerprint);
    let principal = Principal {
        name: "alpha-reader".into(),
        role: Role::Reader,
        namespaces: Some(["alpha".into()].into_iter().collect()),
    };
    let view = principal.view(&snapshot);
    assert_eq!(
        view.route(address, "alpha.test", "/").unwrap().id,
        "alpha/app"
    );
    assert!(view.route(address, "beta.test", "/").is_none());
    assert_eq!(
        snapshot.route(address, "beta.test", "/").unwrap().id,
        "beta/app"
    );
}
fn entry(id: &str, hostname: &str, matcher: serde_json::Value) -> Entry {
    let matcher: Match = serde_json::from_value(matcher).unwrap();
    let path = matcher.path_match(false).unwrap();
    Entry {
        hostname: hostname.into(),
        matcher,
        grpc: false,
        order: ("2026-01-01".into(), "test".into(), id.into(), 0, 0),
        route_id: id.into(),
        route: Arc::new(Route {
            metrics: Default::default(),
            id: id.into(),
            tenant: None,
            rollout: None,
            matcher: path,
            action: Action::Unavailable,
            settings: Settings::default(),
            script: None,
            allowed_backends: Default::default(),
        }),
    }
}
#[test]
fn request_predicates_and_listener_isolation() {
    let address = "127.0.0.1:8080".parse().unwrap();
    let mut routing = Routing {
        listeners: vec![
            ListenerRoutes {
                address,
                hostname: "*.example.test".into(),
                entries: vec![entry("wildcard", "", json!({}))],
            },
            ListenerRoutes {
                address,
                hostname: "api.example.test".into(),
                entries: vec![
                    entry("prefix", "", json!({"path":{"value":"/api"}})),
                    entry(
                        "predicate",
                        "",
                        json!({"path":{"type":"Exact","value":"/api"},"method":"POST","headers":[{"name":"X-Canary","value":"1"}],"queryParams":[{"name":"v","value":"2"}]}),
                    ),
                ],
            },
        ],
        ..Default::default()
    };
    routing.reindex();
    let mut request = RequestData {
        host: "api.example.test".into(),
        path: "/api".into(),
        method: "POST".into(),
        query: "v=2&v=3".into(),
        headers: [("x-canary".into(), "1".into())].into_iter().collect(),
        ..Default::default()
    };
    assert_eq!(routing.route(address, &request).unwrap().id, "predicate");
    request.query = "v=3&v=2".into();
    assert_eq!(routing.route(address, &request).unwrap().id, "prefix");
    request.path = "/apix".into();
    assert!(routing.route(address, &request).is_none());
    request.host = "a.b.example.test".into();
    assert_eq!(routing.route(address, &request).unwrap().id, "wildcard");
    let previous = routing.clone();
    routing.listeners[1].entries.clear();
    routing.reindex();
    request.host = "api.example.test".into();
    assert!(routing.route(address, &request).is_none());
    request.path = "/api".into();
    assert_eq!(previous.route(address, &request).unwrap().id, "prefix");
}
#[test]
fn invalid_backends_keep_their_weight_and_path_rewrite_preserves_boundaries() {
    let policy = Policy {
        backends: vec![
            (Some("valid".into()), 9),
            (None, 1),
            (Some("disabled".into()), 0),
        ],
        ..Default::default()
    };
    assert_eq!(
        (0..100)
            .filter(|sample| policy.select_at(*sample).is_none())
            .count(),
        10
    );
    let modifier = PathModifier {
        type_: "ReplacePrefixMatch".into(),
        replace_prefix_match: Some("/new/".into()),
        replace_full_path: None,
    };
    modifier.validate(true).unwrap();
    assert!(modifier.validate(false).is_err());
    assert_eq!(
        modifier.apply("/old/a", &PathMatch::IngressPrefix("/old/".into())),
        "/new/a"
    );
    assert_eq!(
        modifier.apply("/old", &PathMatch::IngressPrefix("/old/".into())),
        "/new/"
    );
}

// Preserve the previous resolver as an independent oracle for priority and predicate semantics.
fn linear_matches(matcher: &Match, entry: &Entry, request: &RequestData) -> bool {
    if !entry.route.matcher.matches(&request.path) {
        return false;
    }
    if entry.grpc {
        if request.method != "POST"
            || !request.headers.get("content-type").is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|t| t == "application/grpc" || t.starts_with("application/grpc+"))
            })
        {
            return false;
        }
        let Some((service, method)) = request
            .path
            .strip_prefix('/')
            .and_then(|p| p.split_once('/'))
        else {
            return false;
        };
        if let Some(m) = &matcher.method
            && (m
                .get("service")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v != service)
                || m.get("method")
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| v != method))
        {
            return false;
        }
    } else if matcher
        .method
        .as_ref()
        .and_then(|m| m.as_str())
        .is_some_and(|m| m != request.method)
    {
        return false;
    }
    if matcher
        .headers
        .iter()
        .any(|h| request.headers.get(&h.name.to_ascii_lowercase()) != Some(h.value.as_str()))
    {
        return false;
    }
    let query: Vec<_> = url::form_urlencoded::parse(request.query.as_bytes()).collect();
    matcher.query_params.iter().all(|q| {
        query
            .iter()
            .find(|(name, _)| name == &q.name)
            .is_some_and(|(_, value)| value == &q.value)
    })
}
fn linear_rank(entry: &Entry, host: &str) -> Option<(usize, bool, usize, usize, usize, usize)> {
    let (length, exact) = entry.route.matcher.rank();
    let method = if entry.grpc {
        entry.matcher.method.as_ref().map_or(0, |m| {
            usize::from(m.get("service").is_some()) + usize::from(m.get("method").is_some())
        })
    } else {
        usize::from(entry.matcher.method.is_some())
    };
    Some((
        host_rank(&entry.hostname, host)?,
        exact,
        length,
        method,
        entry.matcher.headers.len(),
        entry.matcher.query_params.len(),
    ))
}
fn linear_route(
    routing: &Routing,
    address: SocketAddr,
    request: &RequestData,
) -> Option<Arc<Route>> {
    let host = request.host.to_ascii_lowercase();
    let listener = routing
        .listeners
        .iter()
        .filter(|l| l.address == address)
        .filter_map(|l| host_rank(&l.hostname, &host).map(|rank| (rank, l)))
        .max_by_key(|(rank, _)| *rank)?
        .1;
    listener
        .entries
        .iter()
        .filter(|e| linear_matches(&e.matcher, e, request))
        .filter_map(|e| linear_rank(e, &host).map(|rank| (rank, e)))
        .max_by(|(rank_a, a), (rank_b, b)| rank_a.cmp(rank_b).then_with(|| b.order.cmp(&a.order)))
        .map(|(_, e)| e.route.clone())
}
