//! Unit tests for the idempotency cache.

use super::reply::VALUE_OVERHEAD_BYTES;
use super::*;
use crate::entity_actor::{EntityEvent, EntityResponse, EntityState};
use std::collections::{BTreeMap, VecDeque};

/// The request every reply in these tests answered.
fn req() -> RequestFingerprint {
    RequestFingerprint::new("Act", &serde_json::json!({}))
}

/// The reply cached for a retry of [`req`], if any.
fn hit(cache: &IdempotencyCache, actor_key: &str, idem_key: &str) -> Option<EntityResponse> {
    match cache.get(actor_key, idem_key, &req()) {
        CachedReply::Replay(response) => Some(*response),
        CachedReply::OtherRequest(action) => panic!("cached for another request: {action}"),
        CachedReply::Miss => None,
    }
}

fn make_response(status: &str) -> EntityResponse {
    EntityResponse {
        success: true,
        refusal: None,
        state: EntityState {
            entity_type: String::new(),
            entity_id: String::new(),
            status: status.to_string(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: serde_json::json!({}),
            events: VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
            last_event: Default::default(),
            status_clock: Default::default(),
            spawn_reservations: Default::default(),
        },
        error: None,
        custom_effects: vec![],
        scheduled_actions: vec![],
        spawn_requests: vec![],
        spec_governed: true,
        refused: false,
        answered_action: None,
        trigger_failures: Vec::new(),
        store_unavailable: false,
        unrecorded_at: None,
    }
}

/// A busy entity's reply: large fields and a full window of recent
/// events, each carrying the large parameters that wrote them.
fn busy_response(field_bytes: usize) -> EntityResponse {
    let mut response = make_response("Running");
    response.state.fields = serde_json::json!({ "queue": "q".repeat(field_bytes) });
    for index in 0..50u64 {
        response.state.events.push_back(EntityEvent {
            action: "Stepped".to_string(),
            from_status: "Running".to_string(),
            to_status: "Running".to_string(),
            timestamp: sim_now(),
            params: serde_json::json!({ "queue": "q".repeat(field_bytes) }),
            idempotency_key: Some(format!("dispatch:{index}")),
            origin: Default::default(),
            kind: temper_runtime::persistence::EventKind::Transition,
            outcome: None,
        });
        response.state.processed_idempotency_keys.insert(
            format!("dispatch:{index}"),
            crate::entity_actor::ProcessedIdempotencyKey {
                sequence: index,
                action: Some("Stepped".to_string()),
            },
        );
    }
    response
}

#[test]
fn put_then_get_returns_cached() {
    let cache = IdempotencyCache::new();
    let resp = make_response("Active");
    cache.put("Order:o1", "key-1", &req(), &resp);
    let cached = hit(&cache, "Order:o1", "key-1");
    assert!(cached.is_some());
    assert_eq!(cached.unwrap().state.status, "Active");
}

#[test]
fn pending_effects_do_not_satisfy_protocol_cache_hit() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", &req(), &make_response("Active"));

    assert!(hit(&cache, "Order:o1", "key-1").is_some());
    assert!(
        cache
            .get_after_effects_applied("Order:o1", "key-1", &req())
            .is_none()
    );

    cache.put_effects_applied("Order:o1", "key-1", &req(), &make_response("Active"));
    assert!(
        cache
            .get_after_effects_applied("Order:o1", "key-1", &req())
            .is_some()
    );
}

#[test]
fn a_key_answers_only_the_request_it_was_used_for() {
    let cache = IdempotencyCache::new();
    cache.put_effects_applied("Ticket:t1", "k1", &req(), &make_response("Draft"));

    let close = RequestFingerprint::new("Close", &serde_json::json!({}));
    assert!(matches!(
        cache.get("Ticket:t1", "k1", &close),
        CachedReply::OtherRequest(action) if action == "Act"
    ));
    let other_body = RequestFingerprint::new("Act", &serde_json::json!({"n": 2}));
    assert!(matches!(
        cache.get("Ticket:t1", "k1", &other_body),
        CachedReply::OtherRequest(_)
    ));
    assert!(
        cache
            .get_after_effects_applied("Ticket:t1", "k1", &close)
            .is_none(),
        "the protocol boundary does not answer another request with this reply"
    );
    assert!(hit(&cache, "Ticket:t1", "k1").is_some());
}

#[tokio::test]
async fn a_retry_waits_for_the_running_effects_and_answers_with_their_reply() {
    use futures_util::FutureExt;

    let cache = IdempotencyCache::new();
    cache.put("Job:j1", "k", &req(), &make_response("Pending"));
    let EffectsClaim::Owner(owner) = cache.claim_effects("Job:j1", "k").await else {
        panic!("the first dispatcher runs the effects");
    };
    let waiting = cache.claim_effects("Job:j1", "k");
    tokio::pin!(waiting);
    assert!(
        waiting.as_mut().now_or_never().is_none(),
        "a retry waits while the effects run"
    );

    owner.complete(&req(), &make_response("Started"));
    match waiting.await {
        EffectsClaim::Applied(reply) => assert_eq!(reply.state.status, "Started"),
        EffectsClaim::Owner(_) => panic!("the effects ran; the retry must not run them again"),
    }
}

#[tokio::test]
async fn a_retry_after_the_effects_ran_answers_with_their_reply() {
    // The retry's actor reply is the one cached before the effects ran; by
    // the time it claims them, they have run and completed a later reply.
    let cache = IdempotencyCache::new();
    cache.put("Job:j1", "k", &req(), &make_response("Pending"));
    let EffectsClaim::Owner(owner) = cache.claim_effects("Job:j1", "k").await else {
        panic!("the first dispatcher runs the effects");
    };
    owner.complete(&req(), &make_response("Started"));
    match cache.claim_effects("Job:j1", "k").await {
        EffectsClaim::Applied(reply) => assert_eq!(
            reply.state.status, "Started",
            "the retry answers with the completed reply, not the one before the effects"
        ),
        EffectsClaim::Owner(_) => panic!("the effects ran; the retry must not run them again"),
    }
}

#[tokio::test]
async fn a_waiting_retry_sees_the_effects_ran_when_the_cache_no_longer_holds_the_reply() {
    use futures_util::FutureExt;

    // A cache that keeps no reply: as when other writes evict the completed
    // reply before the waiting retry wakes.
    let cache = IdempotencyCache::with_budget_bytes(0);
    let EffectsClaim::Owner(owner) = cache.claim_effects("Job:j1", "k").await else {
        panic!("the first dispatcher runs the effects");
    };
    let waiting = cache.claim_effects("Job:j1", "k");
    tokio::pin!(waiting);
    assert!(waiting.as_mut().now_or_never().is_none());

    owner.complete(&req(), &make_response("Started"));
    match waiting.await {
        EffectsClaim::Applied(reply) => assert_eq!(reply.state.status, "Started"),
        EffectsClaim::Owner(_) => {
            panic!("the effects ran; the waiting retry must not run them again")
        }
    }
}

#[tokio::test]
async fn a_retry_takes_over_effects_whose_dispatcher_ended_without_them() {
    let cache = IdempotencyCache::new();
    cache.put("Job:j1", "k", &req(), &make_response("Pending"));
    let EffectsClaim::Owner(owner) = cache.claim_effects("Job:j1", "k").await else {
        panic!("the first dispatcher runs the effects");
    };
    let waiting = cache.claim_effects("Job:j1", "k");
    tokio::pin!(waiting);
    drop(owner);
    assert!(matches!(waiting.await, EffectsClaim::Owner(_)));
}

#[test]
fn put_effects_applied_satisfies_protocol_cache_hit() {
    let cache = IdempotencyCache::new();
    cache.put_effects_applied("Order:o1", "key-1", &req(), &make_response("Active"));

    let cached = cache.get_after_effects_applied("Order:o1", "key-1", &req());
    assert_eq!(cached.unwrap().state.status, "Active");
}

#[test]
fn get_missing_returns_none() {
    let cache = IdempotencyCache::new();
    assert!(hit(&cache, "Order:o1", "no-such-key").is_none());
}

#[test]
fn different_actors_isolated() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", &req(), &make_response("A"));
    cache.put("Order:o2", "key-1", &req(), &make_response("B"));
    assert_eq!(hit(&cache, "Order:o1", "key-1").unwrap().state.status, "A");
    assert_eq!(hit(&cache, "Order:o2", "key-1").unwrap().state.status, "B");
}

#[test]
fn budget_evicts_oldest_across_actors() {
    let one = estimated_entry_bytes("actor-00", "k-00", &make_response("S"));
    let cache = IdempotencyCache::with_budget_bytes(one * 10);
    for i in 0..25 {
        cache.put(
            &format!("actor-{i:02}"),
            &format!("k-{i:02}"),
            &req(),
            &make_response("S"),
        );
    }
    assert_eq!(cache.held_bytes(), one * 10);
    assert!(
        hit(&cache, "actor-00", "k-00").is_none(),
        "oldest reply evicted"
    );
    assert!(
        hit(&cache, "actor-14", "k-14").is_none(),
        "oldest replies evicted first"
    );
    assert!(
        hit(&cache, "actor-15", "k-15").is_some(),
        "the ten newest replies kept"
    );
    assert!(
        hit(&cache, "actor-24", "k-24").is_some(),
        "newest reply kept"
    );
    let entries = cache.entries.read().unwrap();
    assert_eq!(entries.len(), 10);
    assert!(
        !entries.by_actor.contains_key("actor-00"),
        "an actor whose last reply is evicted leaves no map behind"
    );
}

#[test]
fn sustained_writes_hold_the_cache_under_budget() {
    let cache = IdempotencyCache::new();
    for i in 0..2_000 {
        cache.put(
            &format!("Feed:feed-{}", i % 25),
            &format!("dispatch:{i}"),
            &req(),
            &busy_response(40 * 1024),
        );
        assert!(cache.held_bytes() <= IDEMPOTENCY_CACHE_BYTES_BUDGET);
    }
    assert!(hit(&cache, "Feed:feed-24", "dispatch:1999").is_some());
}

#[test]
fn cached_reply_keeps_event_metadata_without_parameters() {
    let cache = IdempotencyCache::new();
    let mut response = busy_response(1024);
    response.state.last_event.first_event_at = Some(temper_runtime::scheduler::sim_now());
    cache.put("Feed:f", "k", &req(), &response);
    let cached = hit(&cache, "Feed:f", "k").unwrap();
    assert_eq!(
        cached.state.last_event.first_event_at,
        response.state.last_event.first_event_at
    );
    assert_eq!(cached.state.events.len(), 50);
    let last = cached.state.events.back().unwrap();
    assert_eq!(last.action, "Stepped");
    assert_eq!(last.from_status, "Running");
    assert!(last.params.is_null());
    assert!(cached.state.processed_idempotency_keys.is_empty());
    assert_eq!(
        cached.state.fields["queue"].as_str().map(str::len),
        Some(1024)
    );
}

#[test]
fn replacing_a_reply_frees_its_bytes() {
    let cache = IdempotencyCache::new();
    cache.put("Feed:f", "k", &req(), &busy_response(64 * 1024));
    let first = cache.held_bytes();
    cache.put("Feed:f", "k", &req(), &busy_response(64 * 1024));
    assert_eq!(cache.held_bytes(), first);
}

#[test]
fn reply_larger_than_the_budget_is_not_cached() {
    let cache = IdempotencyCache::with_budget_bytes(4 * 1024);
    cache.put("Feed:f", "k", &req(), &busy_response(8 * 1024));
    assert!(hit(&cache, "Feed:f", "k").is_none());
    assert_eq!(cache.held_bytes(), 0);
}

#[test]
fn replies_waiting_for_their_effects_are_evicted_last() {
    let one = estimated_entry_bytes("actor-00", "k-00", &make_response("S"));
    let cache = IdempotencyCache::with_budget_bytes(one * 10);
    // The oldest reply is still waiting for its post-dispatch effects: a
    // retry needs it, so newer replies whose effects ran go first.
    cache.put("actor-00", "k-00", &req(), &make_response("S"));
    for i in 1..25 {
        cache.put_effects_applied(
            &format!("actor-{i:02}"),
            &format!("k-{i:02}"),
            &req(),
            &make_response("S"),
        );
    }
    assert!(
        hit(&cache, "actor-00", "k-00").is_some(),
        "pending reply kept"
    );
    assert!(
        hit(&cache, "actor-15", "k-15").is_none(),
        "older applied reply evicted"
    );
    assert!(
        hit(&cache, "actor-24", "k-24").is_some(),
        "newest reply kept"
    );

    // Once its effects have run it is an applied reply like the others, and
    // goes when ten newer ones arrive.
    cache.put_effects_applied("actor-00", "k-00", &req(), &make_response("S"));
    for i in 25..35 {
        cache.put_effects_applied(
            &format!("actor-{i:02}"),
            &format!("k-{i:02}"),
            &req(),
            &make_response("S"),
        );
    }
    assert!(hit(&cache, "actor-00", "k-00").is_none());
    assert_eq!(cache.held_bytes(), one * 10);
}

#[test]
fn estimate_counts_every_collection_a_reply_keeps() {
    let bare = estimated_entry_bytes("Feed:f", "k", &make_response("S"));
    let mut response = make_response("S");
    for index in 0..100 {
        response
            .state
            .counters
            .insert(format!("counter-{index:03}"), index);
        response
            .state
            .booleans
            .insert(format!("flag-{index:03}"), true);
        response
            .state
            .lists
            .insert(format!("list-{index:03}"), vec!["item".to_string(); 10]);
        response
            .scheduled_actions
            .push(crate::entity_actor::ScheduledAction {
                action: format!("Timer{index:03}"),
                delay_millis: 5000,
                arm_id: String::new(),
                due_at: None,
            });
    }
    let populated = estimated_entry_bytes("Feed:f", "k", &response);
    // 400 named values and 1000 list items, each at least its overhead.
    assert!(populated >= bare + 1_400 * VALUE_OVERHEAD_BYTES);
}

fn keyed_events(state: &mut EntityState, keys: &[&str]) {
    for key in keys {
        state.events.push_back(EntityEvent {
            action: "Stepped".to_string(),
            from_status: "Running".to_string(),
            to_status: "Running".to_string(),
            timestamp: sim_now(),
            params: serde_json::Value::Null,
            idempotency_key: Some((*key).to_string()),
            origin: Default::default(),
            kind: temper_runtime::persistence::EventKind::Transition,
            outcome: None,
        });
        state.total_event_count += 1;
    }
}

#[test]
fn live_state_answers_a_retry_only_while_it_is_the_reply() {
    let mut cached = make_response("Running").state;
    keyed_events(&mut cached, &["k1", "k2", "k3"]);

    let same = cached.clone();
    assert!(live_state_is_the_reply(&same, &cached, "k3"));

    let mut moved_on = cached.clone();
    keyed_events(&mut moved_on, &["k4"]);
    assert!(
        !live_state_is_the_reply(&moved_on, &cached, "k3"),
        "another action ran after the retried one"
    );

    let mut restored = cached.clone();
    restored.events.clear();
    assert!(
        !live_state_is_the_reply(&restored, &cached, "k3"),
        "restored from a snapshot at the same sequence, with no recent events"
    );

    let mut replayed_tail = cached.clone();
    replayed_tail.events.pop_front();
    replayed_tail.events.pop_front();
    assert!(
        !live_state_is_the_reply(&replayed_tail, &cached, "k3"),
        "restarted from an older snapshot: only the tail was replayed"
    );

    let mut recounted = cached.clone();
    recounted.total_event_count += 1;
    assert!(!live_state_is_the_reply(&recounted, &cached, "k3"));
}

#[path = "index_test.rs"]
mod indexes;
