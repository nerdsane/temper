use super::*;
use crate::entity_actor::{EntityResponse, EntityState};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::future::Future;
use std::task::{Context, Poll, Waker};

fn drop_after_first_pending<F>(future: F)
where
    F: Future<Output = ()>,
{
    let mut future = Box::pin(future);
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
}

fn make_response(status: &str) -> EntityResponse {
    EntityResponse {
        success: true,
        state: EntityState {
            entity_type: String::new(),
            entity_id: String::new(),
            status: status.to_string(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        },
        error: None,
        custom_effects: vec![],
        scheduled_actions: vec![],
        spawn_requests: vec![],
        spec_governed: true,
    }
}

#[test]
fn put_then_get_returns_cached() {
    let cache = IdempotencyCache::new();
    let resp = make_response("Active");
    cache.put("Order:o1", "key-1", resp.clone());
    let cached = cache.get("Order:o1", "key-1");
    assert!(cached.is_some());
    assert_eq!(cached.unwrap().state.status, "Active");
}

#[test]
fn pending_effects_do_not_satisfy_protocol_cache_hit() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", make_response("Active"));

    assert!(cache.get("Order:o1", "key-1").is_some());
    assert!(
        cache
            .get_after_effects_applied("Order:o1", "key-1")
            .is_none()
    );

    assert!(cache.mark_effects_applied("Order:o1", "key-1"));
    assert!(
        cache
            .get_after_effects_applied("Order:o1", "key-1")
            .is_some()
    );
}

#[test]
fn put_effects_applied_satisfies_protocol_cache_hit() {
    let cache = IdempotencyCache::new();
    cache.put_effects_applied("Order:o1", "key-1", make_response("Active"));

    let cached = cache.get_after_effects_applied("Order:o1", "key-1");
    assert_eq!(cached.unwrap().state.status, "Active");
}

#[test]
fn unproved_actor_entry_conflicts_with_bound_action_claim() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", make_response("Active"));

    assert!(matches!(
        cache.lookup_bound_action_replay("Order:o1", "key-1", "request-a"),
        BoundActionReplayLookup::Conflict
    ));
    assert!(matches!(
        cache.claim_bound_action(
            "Order:o1",
            "key-1",
            "request-a",
            &serde_json::json!({"Reason": "a"}),
        ),
        BoundActionClaim::Conflict
    ));
}

#[test]
fn bound_action_claim_serializes_same_key_requests() {
    let cache = IdempotencyCache::new();
    let params = serde_json::json!({"Reason": "a"});
    assert!(matches!(
        cache.claim_bound_action("Order:o1", "key-1", "request-a", &params),
        BoundActionClaim::Claimed
    ));
    assert!(matches!(
        cache.claim_bound_action("Order:o1", "key-1", "request-a", &params),
        BoundActionClaim::Pending
    ));
    assert!(matches!(
        cache.claim_bound_action("Order:o1", "key-1", "request-b", &params),
        BoundActionClaim::Conflict
    ));

    cache.put_bound_action_effects_applied(
        "Order:o1",
        "key-1",
        make_response("Done"),
        "request-a".to_string(),
        params.clone(),
    );
    assert!(matches!(
        cache.claim_bound_action("Order:o1", "key-1", "request-a", &params),
        BoundActionClaim::Pending
    ));
    cache.fail_bound_action_hook("Order:o1", "key-1", "request-a");
    assert!(matches!(
        cache.claim_bound_action("Order:o1", "key-1", "request-a", &params),
        BoundActionClaim::Match { .. }
    ));
}

#[test]
fn in_flight_bound_action_hook_is_not_evicted_by_the_actor_budget() {
    let cache = IdempotencyCache::new();
    let actor_key = "Order:hook-eviction";
    let response = make_response("Done");
    cache.put_bound_action_effects_applied(
        actor_key,
        "000-hook-in-flight",
        response.clone(),
        "request-hook".to_string(),
        serde_json::json!({"Reason": "one hook"}),
    );

    for index in 0..IDEMPOTENCY_BUDGET_PER_ACTOR {
        cache.put_effects_applied(actor_key, &format!("z-fill-{index:04}"), response.clone());
    }

    assert!(
        cache.complete_bound_action_hook(
            actor_key,
            "000-hook-in-flight",
            "request-hook",
            Some(serde_json::json!({"attempt": 1})),
        ),
        "a successful hook must retain its reserved cache entry while it is in flight"
    );
}

fn fill_actor_with_protected_hooks(cache: &IdempotencyCache, actor_key: &str) {
    let response = make_response("Done");
    for index in 0..IDEMPOTENCY_BUDGET_PER_ACTOR {
        let key = format!("protected-{index:04}");
        cache.put_bound_action_effects_applied(
            actor_key,
            &key,
            response.clone(),
            format!("request-{index:04}"),
            serde_json::json!({"index": index}),
        );
    }
}

#[test]
fn protected_entries_keep_claim_admission_within_actor_budget() {
    let cache = IdempotencyCache::new();
    let actor_key = "Order:claim-saturation";
    fill_actor_with_protected_hooks(&cache, actor_key);

    let admission = cache.claim_bound_action(
        actor_key,
        "overflow",
        "request-overflow",
        &serde_json::json!({"Reason": "overflow"}),
    );

    let entries = cache.entries.read().unwrap();
    let actor_entries = entries.get(actor_key).unwrap();
    assert_eq!(actor_entries.len(), IDEMPOTENCY_BUDGET_PER_ACTOR);
    assert!(!matches!(admission, BoundActionClaim::Claimed));
    assert!(!actor_entries.contains_key("overflow"));
}

#[test]
fn protected_entries_keep_response_insertion_within_actor_budget() {
    let cache = IdempotencyCache::new();
    let actor_key = "Order:response-saturation";
    fill_actor_with_protected_hooks(&cache, actor_key);

    assert!(!cache.put_bound_action_effects_applied(
        actor_key,
        "overflow",
        make_response("Done"),
        "request-overflow".to_string(),
        serde_json::json!({"Reason": "overflow"}),
    ));

    let entries = cache.entries.read().unwrap();
    let actor_entries = entries.get(actor_key).unwrap();
    assert_eq!(actor_entries.len(), IDEMPOTENCY_BUDGET_PER_ACTOR);
    assert!(!actor_entries.contains_key("overflow"));
}

#[test]
fn protected_entries_keep_active_claim_under_budget_pressure() {
    let cache = IdempotencyCache::new();
    let actor_key = "Order:active-claim-saturation";
    assert!(matches!(
        cache.claim_bound_action(
            actor_key,
            "active",
            "request-active",
            &serde_json::json!({"Reason": "active"}),
        ),
        BoundActionClaim::Claimed
    ));

    let response = make_response("Done");
    for index in 0..(IDEMPOTENCY_BUDGET_PER_ACTOR - 1) {
        let key = format!("protected-{index:04}");
        cache.put_bound_action_effects_applied(
            actor_key,
            &key,
            response.clone(),
            format!("request-{index:04}"),
            serde_json::json!({"index": index}),
        );
    }

    let admission = cache.claim_bound_action(
        actor_key,
        "overflow",
        "request-overflow",
        &serde_json::json!({"Reason": "overflow"}),
    );
    let entries = cache.entries.read().unwrap();
    let actor_entries = entries.get(actor_key).unwrap();
    assert_eq!(actor_entries.len(), IDEMPOTENCY_BUDGET_PER_ACTOR);
    assert!(actor_entries.contains_key("active"));
    assert!(!actor_entries.contains_key("overflow"));
    assert!(!matches!(admission, BoundActionClaim::Claimed));
}

#[test]
fn active_claim_does_not_age_out_before_its_owner_releases_it() {
    let cache = IdempotencyCache::new();
    let actor_key = "Order:active-claim-ttl";
    let key = "active";
    let fingerprint = "request-active";
    let params = serde_json::json!({"Reason": "active"});
    assert!(matches!(
        cache.claim_bound_action(actor_key, key, fingerprint, &params),
        BoundActionClaim::Claimed
    ));
    cache
        .entries
        .write()
        .unwrap()
        .get_mut(actor_key)
        .unwrap()
        .get_mut(key)
        .unwrap()
        .created_at = sim_now() - chrono::Duration::seconds(IDEMPOTENCY_TTL_SECS + 1);

    assert!(matches!(
        cache.claim_bound_action(actor_key, key, fingerprint, &params),
        BoundActionClaim::Pending
    ));
}

#[test]
fn cancelling_dispatch_await_releases_raw_bound_action_claim() {
    let cache = IdempotencyCache::new();
    let actor_key = "Order:cancelled-dispatch";
    let key = "active";
    let fingerprint = "request-active";
    let params = serde_json::json!({"Reason": "active"});
    assert!(matches!(
        cache.claim_bound_action(actor_key, key, fingerprint, &params),
        BoundActionClaim::Claimed
    ));

    drop_after_first_pending(async {
        let _guard = cache.guard_bound_action_reservation(actor_key, key, fingerprint);
        std::future::pending::<()>().await;
    });

    assert!(matches!(
        cache.claim_bound_action(actor_key, key, fingerprint, &params),
        BoundActionClaim::Claimed
    ));
}

#[test]
fn cancelling_hook_await_releases_owner_and_pins_new_publication_debt() {
    let cache = IdempotencyCache::new();
    let actor_key = "Order:cancelled-hook";
    let key = "active";
    let fingerprint = "request-active";
    assert!(cache.put_bound_action_effects_applied(
        actor_key,
        key,
        make_response("Done"),
        fingerprint.to_string(),
        serde_json::json!({"Reason": "active"}),
    ));
    let publication_gated = Cell::new(false);

    drop_after_first_pending(async {
        let _guard =
            cache.guard_bound_action_hook(actor_key, key, fingerprint, || publication_gated.get());
        publication_gated.set(true);
        std::future::pending::<()>().await;
    });

    let entries = cache.entries.read().unwrap();
    let entry = entries.get(actor_key).unwrap().get(key).unwrap();
    assert!(entry.publication_replay_pinned);
    assert!(!entry.bound_action_hook_in_flight);
    drop(entries);
    assert!(matches!(
        cache.lookup_bound_action_replay(actor_key, key, fingerprint),
        BoundActionReplayLookup::Match { .. }
    ));
}

#[test]
fn publication_replay_pin_survives_ttl_until_completed_and_unpinned() {
    let cache = IdempotencyCache::new();
    let actor = "Order:o1";
    let key = "publication-key";
    let fingerprint = "request-a";
    let params = serde_json::json!({"Reason": "publish"});
    cache.put_bound_action_effects_applied(
        actor,
        key,
        make_response("Done"),
        fingerprint.to_string(),
        params,
    );
    assert!(cache.pin_bound_action_replay(actor, key, fingerprint));
    cache.fail_bound_action_hook(actor, key, fingerprint);
    cache
        .entries
        .write()
        .unwrap()
        .get_mut(actor)
        .unwrap()
        .get_mut(key)
        .unwrap()
        .created_at = sim_now() - chrono::Duration::seconds(IDEMPOTENCY_TTL_SECS + 1);

    assert!(matches!(
        cache.lookup_bound_action_replay(actor, key, fingerprint),
        BoundActionReplayLookup::Match { .. }
    ));

    // An in-flight retry is not enough to discharge the outcome-ambiguous
    // publication debt: cancellation can still leave the hook incomplete.
    cache.unpin_bound_action_replay(actor, key, fingerprint);
    assert!(
        cache
            .entries
            .read()
            .unwrap()
            .get(actor)
            .unwrap()
            .get(key)
            .unwrap()
            .publication_replay_pinned
    );

    assert!(cache.complete_bound_action_hook(actor, key, fingerprint, None));
    assert!(
        !cache
            .entries
            .read()
            .unwrap()
            .get(actor)
            .unwrap()
            .get(key)
            .unwrap()
            .publication_replay_pinned
    );
    cache
        .entries
        .write()
        .unwrap()
        .get_mut(actor)
        .unwrap()
        .get_mut(key)
        .unwrap()
        .created_at = sim_now() - chrono::Duration::seconds(IDEMPOTENCY_TTL_SECS + 1);
    assert!(matches!(
        cache.lookup_bound_action_replay(actor, key, fingerprint),
        BoundActionReplayLookup::Miss
    ));
}

#[test]
fn get_missing_returns_none() {
    let cache = IdempotencyCache::new();
    assert!(cache.get("Order:o1", "no-such-key").is_none());
}

#[test]
fn different_actors_isolated() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", make_response("A"));
    cache.put("Order:o2", "key-1", make_response("B"));
    assert_eq!(cache.get("Order:o1", "key-1").unwrap().state.status, "A");
    assert_eq!(cache.get("Order:o2", "key-1").unwrap().state.status, "B");
}

#[test]
fn budget_evicts_oldest() {
    let cache = IdempotencyCache::new();
    // Fill to budget
    for i in 0..IDEMPOTENCY_BUDGET_PER_ACTOR {
        cache.put("actor", &format!("k-{i}"), make_response("S"));
    }
    // One more should evict the oldest
    cache.put("actor", "k-overflow", make_response("New"));
    let entries = cache.entries.read().unwrap();
    let actor_entries = entries.get("actor").unwrap();
    assert_eq!(actor_entries.len(), IDEMPOTENCY_BUDGET_PER_ACTOR);
    assert!(actor_entries.contains_key("k-overflow"));
}
