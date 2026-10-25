use super::*;

fn state(min_requests: usize, error_percent: u32) -> Arc<State> {
    let policy = Policy::parse(&serde_json::json!({
        "revision":"window-test",
        "backends":[{"service":"stable","weight":1},{"service":"canary","weight":1}],
        "rollback":{"fallback":"stable","min_requests":min_requests,"error_percent":error_percent,"window_seconds":60,"max_p95_ms":100},
        "steps":[
            {"weights":{"stable":1,"canary":1},"duration_seconds":0},
            {"weights":{"stable":1,"canary":1},"duration_seconds":0}
        ]
    }).to_string()).unwrap();
    Rollouts::default().get(
        "ns/window",
        "uid",
        policy,
        BTreeMap::from([
            ("stable".into(), "stable".into()),
            ("canary".into(), "canary".into()),
        ]),
    )
}

fn samples(state: &State) -> (u64, u64, u64) {
    let (_, samples) = state.sample_report();
    (samples.count, samples.errors, samples.slow)
}

#[test]
fn p95_rollback_preserves_nearest_rank_and_strict_threshold_boundaries() {
    let state = state(20, 100);
    for _ in 0..19 {
        assert!(!state.completed("canary", false, Duration::from_millis(100), 0));
    }
    assert!(!state.completed("canary", false, Duration::from_millis(101), 0));
    assert_eq!(samples(&state), (20, 0, 1));
    assert!(!state.completed("canary", false, Duration::from_millis(100), 0));
    assert!(state.completed("canary", false, Duration::from_millis(101), 0));
    assert_eq!(state.select(&Default::default()), "stable");
    assert_eq!(samples(&state), (22, 0, 2));
}

#[test]
fn error_rollback_ignores_stable_and_old_stage_completions_and_includes_equality() {
    let state = state(10, 20);
    assert!(!state.completed("stable", true, Duration::from_secs(1), 0));
    assert!(!state.completed("canary", true, Duration::from_secs(1), 1));
    for _ in 0..8 {
        assert!(!state.completed("canary", false, Duration::ZERO, 0));
    }
    assert!(!state.completed("canary", true, Duration::ZERO, 0));
    assert!(state.completed("canary", true, Duration::ZERO, 0));
    assert_eq!(samples(&state), (10, 2, 0));
}

#[test]
fn sample_capacity_expiry_and_stage_changes_remove_all_old_counts() {
    let state = state(10000, 100);
    assert!(!state.completed("canary", true, Duration::from_millis(101), 0));
    for _ in 0..9999 {
        assert!(!state.completed("canary", false, Duration::ZERO, 0));
    }
    assert_eq!(samples(&state), (10000, 1, 1));
    assert!(!state.completed("canary", false, Duration::ZERO, 0));
    assert_eq!(samples(&state), (10000, 0, 0));
    assert!(!state.completed("canary", true, Duration::from_millis(101), 0));
    let expired = Instant::now() - Duration::from_secs(62);
    for sample in &mut state.window.lock().unwrap().samples {
        sample.0 = expired;
    }
    // Replica reporting must expire samples even when no new request arrives.
    assert_eq!(samples(&state), (0, 0, 0));
    assert!(!state.completed("canary", true, Duration::from_millis(101), 0));
    let previous_key = state.sample_key();
    state
        .sync_progress(Some(
            &serde_json::to_string(&Progress {
                revision: state.policy.revision.clone(),
                policy_hash: state.policy_hash(),
                stage: 1,
                started_at: 1,
                ..Default::default()
            })
            .unwrap(),
        ))
        .unwrap();
    assert_ne!(state.sample_key(), previous_key);
    assert_eq!(samples(&state), (0, 0, 0));
    assert!(!state.completed("canary", true, Duration::from_secs(1), 0));
    assert!(!state.completed("canary", false, Duration::ZERO, 1));
    assert_eq!(samples(&state), (1, 0, 0));
}
