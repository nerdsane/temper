//! Focused tests for durable timeout hydration helpers and arm races.

use super::*;

fn key() -> EntityKey {
    EntityKey {
        tenant: "t".into(),
        entity_type: "E".into(),
        entity_id: "1".into(),
    }
}

#[test]
fn dispatch_arm_wins_when_it_precedes_hydration_reconciliation() {
    let tracker = StateTimeoutTracker::new();
    let entity = key();

    let dispatch_seq = tracker.bump(&entity);
    assert_eq!(tracker.reserve_if_unarmed(&entity), None);
    assert_eq!(
        tracker.current(&entity),
        dispatch_seq,
        "late hydration must not invalidate the live dispatch deadline"
    );
}

#[test]
fn dispatch_arm_supersedes_an_earlier_hydration_reservation() {
    let tracker = StateTimeoutTracker::new();
    let entity = key();

    let hydration_seq = tracker
        .reserve_if_unarmed(&entity)
        .expect("hydration claims an unarmed entity");
    let dispatch_seq = tracker.bump(&entity);
    assert_ne!(hydration_seq, dispatch_seq);
    assert_eq!(
        tracker.current(&entity),
        dispatch_seq,
        "a real transition must retain the only current deadline"
    );
}

#[test]
fn hydration_delay_seed_sweep_covers_remaining_exact_and_overdue_budgets() {
    let budget = Duration::from_secs(60);
    let entry = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    let mut events = VecDeque::new();
    events.push_back(EntityEvent {
        action: "Start".to_string(),
        from_status: "Idle".to_string(),
        to_status: "Running".to_string(),
        timestamp: entry,
        params: serde_json::json!({}),
        idempotency_key: None,
    });

    let mut saw_remaining = false;
    let mut saw_exact = false;
    let mut saw_overdue = false;
    for seed in 0_u64..128 {
        let elapsed_secs = seed.wrapping_mul(37) % 121;
        let now = entry + chrono::Duration::seconds(elapsed_secs as i64);
        let hydration =
            compute_hydration_delay(&events, None, "Running", &[], budget, now).unwrap();
        assert_eq!(
            hydration.delay,
            budget.saturating_sub(Duration::from_secs(elapsed_secs)),
            "seed {seed} must recover the exact remaining budget"
        );
        assert_eq!(hydration.overdue, elapsed_secs >= 60);
        saw_remaining |= elapsed_secs < 60;
        saw_exact |= elapsed_secs == 60;
        saw_overdue |= elapsed_secs > 60;
    }

    assert!(saw_remaining, "seed sweep must cover a remaining budget");
    assert!(saw_exact, "seed sweep must cover the exact deadline");
    assert!(saw_overdue, "seed sweep must cover overdue recovery");
}

#[test]
fn reconciliation_charges_only_time_after_a_later_durable_entry() {
    let observed_at = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    let entered_at = observed_at + chrono::Duration::seconds(3);
    let events = VecDeque::from([EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Running".to_string(),
        timestamp: entered_at,
        params: serde_json::json!({}),
        idempotency_key: None,
    }]);
    let reconciled_at = hydration_reconciled_at(observed_at, Duration::from_secs(5));

    assert_eq!(
        compute_hydration_delay(
            &events,
            Some(entered_at),
            "Running",
            &[],
            Duration::from_secs(60),
            reconciled_at,
        ),
        Some(HydrationDelay {
            delay: Duration::from_secs(58),
            overdue: false,
        }),
        "readiness before the durable Created event must not consume its timeout budget"
    );
}

#[test]
fn snapshot_anchor_survives_an_empty_recent_event_window() {
    let reset_at = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    assert_eq!(
        compute_state_clock_reset_ts(&VecDeque::new(), Some(reset_at), "Running", &[]),
        Some(reset_at),
        "a current snapshot must retain the durable timeout anchor"
    );
}

#[tokio::test(start_paused = true)]
async fn absolute_deadline_survives_timer_task_poll_delay() {
    let deadline = timeout_deadline(Duration::from_secs(10));

    // Model a spawned timer task that receives no CPU for four seconds.
    tokio::time::advance(Duration::from_secs(4)).await;
    let timer = tokio::spawn(async move { tokio::time::sleep_until(deadline).await });
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_millis(5_999)).await;
    tokio::task::yield_now().await;
    assert!(
        !timer.is_finished(),
        "the timer must not fire before its deadline"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    timer
        .await
        .expect("task queue time must not move the precomputed deadline later");
}
