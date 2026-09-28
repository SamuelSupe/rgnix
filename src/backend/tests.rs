use super::*;
use sha2::{Digest, Sha256};

fn legacy_choice(key: &str, endpoints: &[Endpoint]) -> Option<SocketAddr> {
    let score = |endpoint: &Endpoint| {
        let bytes = Sha256::digest(format!("{key}\0{}", endpoint.address).as_bytes());
        let value = u64::from_be_bytes(bytes[..8].try_into().unwrap());
        let uniform = (value as f64 + 1.0) / (u64::MAX as f64 + 2.0);
        -uniform.ln() / f64::from(endpoint.weight)
    };
    endpoints
        .iter()
        .filter(|e| e.weight > 0)
        .min_by(|a, b| score(a).total_cmp(&score(b)))
        .map(|e| e.address)
}

#[test]
fn hash_and_sticky_preserve_existing_assignments() {
    let endpoints: Vec<_> = [
        ("127.0.0.1:80", 1),
        ("192.0.2.1:65535", 3),
        ("[2001:db8::1]:443", 5),
        ("[fe80::1%3]:8080", 1),
        ("127.0.0.2:80", 0),
    ]
    .into_iter()
    .map(|(address, weight)| Endpoint {
        address: address.parse().unwrap(),
        weight,
    })
    .collect();
    let keys: Vec<_> = ["", "tenant-", "租户\0", &"x".repeat(4096)]
        .into_iter()
        .flat_map(|prefix| (0..256).map(move |i| format!("{prefix}{i}")))
        .chain([String::new()])
        .collect();
    for balance in [
        Balance::Hash(Key::Cookie("session".into())),
        Balance::Sticky("session".into()),
    ] {
        for entries in [vec![], endpoints[..1].to_vec(), endpoints.clone()] {
            let mut backend = Backend::new(entries.clone(), false, "test".into(), "test".into());
            backend.options.balance = balance.clone();
            let backend = Arc::new(backend);
            for key in &keys {
                assert_eq!(
                    backend.select(key).map(|lease| lease.address),
                    legacy_choice(key, &entries)
                );
            }
            assert_eq!(backend.active.load(Ordering::Relaxed), 0);
        }
    }
}

#[test]
fn hashed_selection_preserves_health_and_lease_limits() {
    let endpoints: Vec<_> = (1..=4)
        .map(|i| Endpoint {
            address: format!("127.0.0.{i}:80").parse().unwrap(),
            weight: i,
        })
        .collect();
    let mut backend = Backend::new(endpoints.clone(), false, "test".into(), "test".into());
    backend.options.balance = Balance::Hash(Key::Header("session".into()));
    backend.options.max_inflight = 1;
    backend.options.health = Some(HealthCheck {
        path: "/health".into(),
        interval: Duration::from_secs(1),
        timeout: Duration::from_millis(100),
        status: 200,
    });
    let backend = Arc::new(backend);
    assert!(backend.select("session").is_none());
    {
        let pool = backend.pool.lock().unwrap();
        for (_, state) in pool.endpoints.iter() {
            state.set_active_health(true, &backend.health_revision);
        }
    }
    let selected = legacy_choice("session", &endpoints).unwrap();
    let lease = backend.select("session").unwrap();
    assert_eq!(lease.address, selected);
    assert!(backend.select("another").is_none());
    assert!(lease.record_result(true, 1, Duration::from_secs(60)));
    drop(lease);
    let remaining: Vec<_> = endpoints
        .iter()
        .filter(|e| e.address != selected)
        .cloned()
        .collect();
    for i in 0..256 {
        let key = format!("session-{i}");
        assert_eq!(
            backend.select(&key).unwrap().address,
            legacy_choice(&key, &remaining).unwrap()
        );
    }
    {
        let pool = backend.pool.lock().unwrap();
        for (endpoint, state) in pool.endpoints.iter() {
            if endpoint.address != selected {
                state.set_active_health(false, &backend.health_revision);
            }
        }
    }
    backend.record_result(selected, false, 1, Duration::from_secs(60));
    let lease = backend.select("session").unwrap();
    assert_eq!(lease.address, selected);
    assert!(!lease.record_result(false, 1, Duration::from_secs(60)));
    drop(lease);
    assert_eq!(backend.active.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn dns_ttl_refresh_replaces_endpoints_without_invalidating_active_leases() -> Result<()> {
    use hickory_resolver::{
        config::{LookupIpStrategy, NameServerConfigGroup, ResolverConfig},
        name_server::TokioConnectionProvider,
    };
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let address = socket.local_addr()?;
    let generation = Arc::new(AtomicUsize::new(1));
    let answer = generation.clone();
    let task = tokio::spawn(async move {
        let mut buffer = [0; 512];
        loop {
            let Ok((length, peer)) = socket.recv_from(&mut buffer).await else {
                break;
            };
            let mut end = 12;
            while buffer[end] != 0 {
                end += 1 + usize::from(buffer[end]);
            }
            end += 5;
            if end > length {
                continue;
            }
            let mut response = buffer[..end].to_vec();
            response[2..12].copy_from_slice(&[0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]);
            response.extend_from_slice(&[
                0xc0,
                0x0c,
                0,
                1,
                0,
                1,
                0,
                0,
                0,
                1,
                0,
                4,
                127,
                0,
                0,
                answer.load(Ordering::Relaxed) as u8,
            ]);
            let _ = socket.send_to(&response, peer).await;
        }
    });
    let config = ResolverConfig::from_parts(
        None,
        vec![],
        NameServerConfigGroup::from_ips_clear(&[address.ip()], address.port(), true),
    );
    let mut builder =
        hickory_resolver::Resolver::builder_with_config(config, TokioConnectionProvider::default());
    builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4Only;
    let resolver = builder.build();
    let mut backend = Backend::new(vec![], false, "backend.test".into(), "backend.test".into());
    backend.origins = vec![Origin {
        host: "backend.test".into(),
        port: 8080,
        weight: 1,
    }];
    backend.origins.push(Origin {
        host: "127.0.0.9".into(),
        port: 8080,
        weight: 1,
    });
    backend.options.balance = Balance::Hash(Key::Ip);
    let dynamic_address = |generation| format!("127.0.0.{generation}:8080").parse().unwrap();
    let key = (0..1000)
        .map(|i| i.to_string())
        .find(|key| {
            [1, 2].into_iter().all(|generation| {
                legacy_choice(
                    key,
                    &[
                        Endpoint {
                            address: dynamic_address(generation),
                            weight: 1,
                        },
                        Endpoint {
                            address: dynamic_address(9),
                            weight: 1,
                        },
                    ],
                ) == Some(dynamic_address(generation))
            })
        })
        .unwrap();
    let backend = Arc::new(backend);
    backend.maintain(Some(&resolver)).await;
    let lease = backend.select(&key).expect("initial DNS answer");
    let snapshot = backend.pool.lock().unwrap().selection.clone().unwrap();
    assert_eq!(lease.address, "127.0.0.1:8080".parse()?);
    generation.store(2, Ordering::Relaxed);
    backend.maintain(Some(&resolver)).await;
    assert_eq!(backend.select(&key).unwrap().address, lease.address);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    backend.maintain(Some(&resolver)).await;
    assert_eq!(
        backend.select(&key).unwrap().address,
        "127.0.0.2:8080".parse()?
    );
    assert_eq!(
        backend.select_from(&snapshot, &key).unwrap().address,
        lease.address
    );
    assert_eq!(lease.address, "127.0.0.1:8080".parse()?);
    assert!(lease.record_result(true, 1, Duration::from_secs(60)));
    assert_eq!(
        backend.select(&key).unwrap().address,
        "127.0.0.2:8080".parse()?
    );
    drop(lease);
    assert_eq!(backend.active.load(Ordering::Relaxed), 0);
    task.abort();
    Ok(())
}

#[test]
fn concurrent_selection_respects_capacity_and_least_connection_distribution() {
    use std::{sync::Barrier, thread};
    for balance in [
        Balance::RoundRobin,
        Balance::LeastConnections,
        Balance::Hash(Key::Ip),
        Balance::Sticky("session".into()),
    ] {
        let endpoints = (1..=8)
            .map(|i| Endpoint {
                address: format!("127.0.0.{i}:80").parse().unwrap(),
                weight: 1,
            })
            .collect();
        let mut backend = Backend::new(endpoints, false, "test".into(), "test".into());
        backend.options.balance = balance.clone();
        backend.options.max_inflight = 8;
        let backend = Arc::new(backend);
        let barrier = Barrier::new(32);
        for _ in 0..16 {
            let leases = thread::scope(|scope| {
                let jobs: Vec<_> = (0..32)
                    .map(|i| {
                        let (backend, barrier) = (&backend, &barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            backend.select(&i.to_string())
                        })
                    })
                    .collect();
                jobs.into_iter()
                    .filter_map(|job| job.join().unwrap())
                    .collect::<Vec<_>>()
            });
            assert_eq!(leases.len(), 8, "{balance:?}");
            assert_eq!(backend.observation().active, 8);
            assert!(backend.select("full").is_none());
            if balance == Balance::LeastConnections {
                let addresses: std::collections::BTreeSet<_> =
                    leases.iter().map(|l| l.address).collect();
                assert_eq!(addresses.len(), 8);
            }
            drop(leases);
            assert_eq!(backend.observation().active, 0);
            assert_eq!(
                backend
                    .pool
                    .lock()
                    .unwrap()
                    .endpoints
                    .iter()
                    .map(|(_, s)| s.active.load(Ordering::Relaxed))
                    .sum::<usize>(),
                0
            );
        }
    }
}

#[test]
fn weighted_round_robin_preserves_sequence_across_health_changes() {
    let endpoints: Vec<_> = [0, 1, 3, 7]
        .into_iter()
        .enumerate()
        .map(|(i, weight)| Endpoint {
            address: format!("127.0.0.{}:80", i + 1).parse().unwrap(),
            weight,
        })
        .collect();
    let mut backend = Backend::new(endpoints.clone(), false, "test".into(), "test".into());
    backend.options.health = Some(HealthCheck {
        path: "/health".into(),
        interval: Duration::from_secs(1),
        timeout: Duration::from_millis(100),
        status: 200,
    });
    let backend = Arc::new(backend);
    assert!(backend.select("").is_none());
    let states = backend.pool.lock().unwrap().endpoints.clone();
    for (_, state) in states.iter() {
        state.set_active_health(true, &backend.health_revision);
    }
    let mut cursor = u64::MAX - 10;
    backend.cursor.store(cursor, Ordering::Relaxed);
    for stage in 0..5 {
        match stage {
            1 => assert!(backend.record_result(
                endpoints[2].address,
                true,
                1,
                Duration::from_secs(60)
            )),
            2 => states[3]
                .1
                .set_active_health(false, &backend.health_revision),
            3 => {
                backend.record_result(endpoints[2].address, false, 1, Duration::from_secs(60));
            }
            4 => states[3]
                .1
                .set_active_health(true, &backend.health_revision),
            _ => {}
        }
        let slots: Vec<_> = endpoints
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                !((1..=2).contains(&stage) && *i == 2 || (2..=3).contains(&stage) && *i == 3)
            })
            .flat_map(|(_, e)| std::iter::repeat_n(e.address, e.weight as usize))
            .collect();
        for _ in 0..100 {
            assert_eq!(
                backend.select("").unwrap().address,
                slots[(cursor % slots.len() as u64) as usize]
            );
            cursor = cursor.wrapping_add(1);
        }
    }
}

#[test]
fn empty_selection_recovers_after_cooldown_without_maintenance() {
    for balance in [
        Balance::RoundRobin,
        Balance::LeastConnections,
        Balance::Hash(Key::Ip),
        Balance::Sticky("session".into()),
    ] {
        let endpoints: Vec<_> = (1..=2)
            .map(|i| Endpoint {
                address: format!("127.0.0.{i}:80").parse().unwrap(),
                weight: u32::MAX,
            })
            .collect();
        let mut backend = Backend::new(endpoints.clone(), false, "test".into(), "test".into());
        backend.options.balance = balance;
        let backend = Arc::new(backend);
        assert!(backend.select("warm").is_some());
        assert!(backend.record_result(endpoints[0].address, true, 1, Duration::from_secs(60)));
        assert!(backend.record_result(endpoints[1].address, true, 1, Duration::from_millis(50)));
        assert!(backend.select("blocked").is_none());
        std::thread::sleep(Duration::from_millis(70));
        for _ in 0..100 {
            assert_eq!(
                backend.select("recovered").unwrap().address,
                endpoints[1].address
            );
        }
        assert!(!backend.record_result(endpoints[0].address, false, 1, Duration::from_secs(60)));
        assert_eq!(backend.observation().eligible, 2);
        assert!(backend.select("both").is_some());
        assert_eq!(backend.observation().active, 0);
    }
}
