use super::*;
use crate::entity_actor::{EntityResponse, EntityState};
use std::collections::BTreeMap;

pub(super) fn make_response(status: &str) -> EntityResponse {
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

fn hit_status(lookup: IdempotencyLookup) -> Option<String> {
    match lookup {
        IdempotencyLookup::Hit(response) => Some(response.state.status),
        _ => None,
    }
}

const B: &str = "binding-a";

#[test]
fn put_then_lookup_returns_cached() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("Active"));
    assert_eq!(
        hit_status(cache.lookup("Order:o1", "key-1", B)).as_deref(),
        Some("Active")
    );
}

#[test]
fn different_binding_is_mismatch_not_hit() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("Active"));
    assert!(matches!(
        cache.lookup("Order:o1", "key-1", "binding-b"),
        IdempotencyLookup::Mismatch
    ));
    // Mismatch is reported even before effects are marked applied.
    assert!(matches!(
        cache.lookup_after_completion("Order:o1", "key-1", "binding-b"),
        IdempotencyLookup::Mismatch
    ));
}

#[test]
fn pending_effects_do_not_satisfy_protocol_cache_hit() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("Active"));

    assert!(hit_status(cache.lookup("Order:o1", "key-1", B)).is_some());
    assert!(matches!(
        cache.lookup_after_completion("Order:o1", "key-1", B),
        IdempotencyLookup::Miss
    ));

    owner(&cache).complete(CompletionResult::response(make_response("Active")));
    assert!(hit_status(cache.lookup_after_completion("Order:o1", "key-1", B)).is_some());
}

#[test]
fn lookup_missing_is_miss() {
    let cache = IdempotencyCache::new();
    assert!(matches!(
        cache.lookup("Order:o1", "no-such-key", B),
        IdempotencyLookup::Miss
    ));
}

#[test]
fn different_actors_isolated() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("A"));
    cache.put("Order:o2", "key-1", "binding-b", make_response("B"));
    assert_eq!(
        hit_status(cache.lookup("Order:o1", "key-1", B)).as_deref(),
        Some("A")
    );
    assert_eq!(
        hit_status(cache.lookup("Order:o2", "key-1", "binding-b")).as_deref(),
        Some("B")
    );
}

#[test]
fn budget_evicts_oldest() {
    let cache = IdempotencyCache::new();
    for i in 0..IDEMPOTENCY_BUDGET_PER_ACTOR {
        cache.put("actor", &format!("k-{i}"), B, make_response("S"));
    }
    cache.put("actor", "k-overflow", B, make_response("New"));
    let entries = cache.entries.read().unwrap();
    let actor_entries = entries.get("actor").unwrap();
    assert_eq!(actor_entries.len(), IDEMPOTENCY_BUDGET_PER_ACTOR);
    assert!(actor_entries.contains_key("k-overflow"));
}

#[test]
fn binding_ignores_qualifier_nested_key_order_and_top_level_server_fields() {
    let a = serde_json::json!({"Q": 1, "Meta": {"x": 1, "y": [1, 2]}});
    let b = serde_json::json!({"Id": "o1", "Meta": {"y": [1, 2], "x": 1}, "Q": 1});
    assert_eq!(
        request_binding("Temper.Example.AddItem", &a),
        request_binding("AddItem", &b)
    );
}

#[test]
fn binding_is_sensitive_to_action_arrays_values_and_nested_odata_keys() {
    let base = serde_json::json!({"Q": 1, "Meta": {"x": 1, "y": [1, 2]}});
    let reference = request_binding("AddItem", &base);
    assert_ne!(reference, request_binding("CancelOrder", &base));
    for other in [
        serde_json::json!({"Q": 2, "Meta": {"x": 1, "y": [1, 2]}}),
        serde_json::json!({"Q": 1, "Meta": {"x": 1, "y": [2, 1]}}),
        serde_json::json!({"Q": 1, "Meta": {"x": 1, "y": [1, 2], "@odata.type": "#T"}}),
        serde_json::json!({"Q": 1, "Meta": {"x": 1, "y": [1, 2], "Id": "nested"}}),
        serde_json::json!({"Q": "1", "Meta": {"x": 1, "y": [1, 2]}}),
    ] {
        assert_ne!(reference, request_binding("AddItem", &other), "{other}");
    }
}

fn claim(cache: &IdempotencyCache, actor: &str, key: &str) -> OperationClaim {
    cache.claim_operation(
        actor,
        key,
        OperationRequest {
            tenant: temper_runtime::tenant::TenantId::default(),
            entity_type: "Order".into(),
            entity_id: "o1".into(),
            action: "Start".into(),
            params: serde_json::json!({}),
            agent_ctx: crate::request_context::AgentContext::system(),
            idempotency_key: Some(key.into()),
            await_integration: true,
            reaction_depth: None,
            detach_reactions: false,
        },
    )
}

fn owner(cache: &IdempotencyCache) -> OperationOwner {
    match claim(cache, "Order:o1", "key-1") {
        OperationClaim::Owner(owner) => owner,
        _ => panic!("expected exclusive ownership"),
    }
}

fn waiter(cache: &IdempotencyCache) -> Arc<OperationReceipt> {
    match claim(cache, "Order:o1", "key-1") {
        OperationClaim::Join(receipt) => receipt,
        _ => panic!("expected existing receipt"),
    }
}

async fn wait(receipt: Arc<OperationReceipt>) -> EntityResponse {
    receipt
        .wait(
            true,
            false,
            &crate::request_context::AgentContext::system(),
            false,
        )
        .await
        .response
}

#[tokio::test]
async fn exactly_one_dispatcher_owns_fresh_commit_effects_and_publishes_final_response() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("Running"));
    let first = owner(&cache);
    let waiting = waiter(&cache);
    assert!(matches!(
        cache.lookup_after_completion("Order:o1", "key-1", B),
        IdempotencyLookup::Miss
    ));
    first.complete(CompletionResult::response(make_response("Done")));
    assert_eq!(wait(waiting).await.state.status, "Done");
    assert_eq!(
        hit_status(cache.lookup_after_completion("Order:o1", "key-1", B)).as_deref(),
        Some("Done")
    );
    let OperationClaim::Join(receipt) = claim(&cache, "Order:o1", "key-1") else {
        panic!("completed effects must replay")
    };
    assert_eq!(
        receipt.terminal().unwrap().state.status,
        "Done",
        "a dispatcher with an older actor reply still receives the final result"
    );
}

#[tokio::test]
async fn failed_attempt_releases_pending_but_existing_waiters_keep_its_failure() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("Running"));
    let first = owner(&cache);
    let waiting = waiter(&cache);
    let mut failed = make_response("Running");
    failed.success = false;
    failed.error = Some("injected effects failure".into());
    first.complete(CompletionResult::response(failed.clone()));
    assert_eq!(
        hit_status(cache.lookup("Order:o1", "key-1", B)).as_deref(),
        Some("Running")
    );
    owner(&cache).complete(CompletionResult::response(make_response("Done")));
    let response = wait(waiting).await;
    assert!(!response.success);
    assert_eq!(response.error, failed.error);
}

#[tokio::test]
async fn worker_termination_is_not_reclaimable_and_notifies_waiters() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("Running"));
    let first = owner(&cache);
    let permit = cache
        .continuation_slots
        .clone()
        .try_acquire_owned()
        .unwrap();
    first.receipt.retain_permit(Arc::new(permit));
    let waiting = waiter(&cache);
    drop(first);
    let response = wait(waiting).await;
    assert!(!response.success);
    assert!(response.error.unwrap().contains("completion is unknown"));
    assert!(matches!(
        claim(&cache, "Order:o1", "key-1"),
        OperationClaim::Join(_)
    ));
    assert_eq!(cache.continuation_slots.available_permits(), 63);
}

#[tokio::test]
async fn active_effects_survive_ttl_eviction_and_replacement() {
    let (_guard, clock, _) = temper_runtime::scheduler::install_deterministic_context(51930);
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("Running"));
    let first = owner(&cache);
    let waiting = waiter(&cache);
    clock.advance_by((IDEMPOTENCY_TTL_SECS as u64 + 1) * 10);
    for i in 0..=IDEMPOTENCY_BUDGET_PER_ACTOR {
        cache.put("Order:o1", &format!("other-{i}"), B, make_response("Other"));
    }
    cache.put_historical("Order:o1", "key-1", "different", make_response("Old"));
    assert!(matches!(
        cache.lookup("Order:o1", "key-1", "different"),
        IdempotencyLookup::Mismatch
    ));
    assert!(matches!(
        cache.lookup("Order:o1", "key-1", B),
        IdempotencyLookup::Hit(_)
    ));
    assert!(matches!(
        claim(&cache, "Order:o1", "key-1"),
        OperationClaim::Join(_)
    ));
    first.complete(CompletionResult::response(make_response("Done")));
    assert_eq!(wait(waiting).await.state.status, "Done");
    assert_eq!(
        hit_status(cache.lookup_after_completion("Order:o1", "key-1", B)).as_deref(),
        Some("Done")
    );
}

#[test]
fn historical_replays_never_own_transition_effects() {
    let cache = IdempotencyCache::new();
    cache.put_historical("Order:o1", "key-1", B, make_response("Active"));
    assert!(matches!(
        claim(&cache, "Order:o1", "key-1"),
        OperationClaim::Historical
    ));
    assert!(matches!(
        cache.lookup_after_completion("Order:o1", "key-1", B),
        IdempotencyLookup::Miss
    ));
    assert!(matches!(
        claim(&cache, "Order:o2", "absent"),
        OperationClaim::Uncached
    ));
}

#[tokio::test]
async fn unstarted_reactions_resume_without_rerunning_completed_effects() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("Running"));
    let first = owner(&cache);
    let receipt = first.receipt.clone();
    first.publish_effects(make_response("EffectsReady"));
    assert!(matches!(
        cache.lookup_after_completion("Order:o1", "key-1", B),
        IdempotencyLookup::Miss
    ));
    drop(first);
    assert!(
        !wait(receipt.clone()).await.success,
        "ended continuation must notify terminal waiters"
    );
    let resumed = owner(&cache);
    assert!(Arc::ptr_eq(&receipt, &resumed.receipt));
    assert!(resumed.receipt.phase() == OperationPhase::PendingReactions);
    let response = resumed.start_reactions();
    assert_eq!(response.state.status, "EffectsReady");
    resumed.complete(CompletionResult::response(response));
    assert_eq!(wait(receipt).await.state.status, "EffectsReady");
}

#[tokio::test]
async fn interrupted_reactions_preserve_ready_effects_but_pin_the_root_slot() {
    let (_guard, clock, _) = temper_runtime::scheduler::install_deterministic_context(51976);
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", B, make_response("Running"));
    let first = owner(&cache);
    let receipt = first.receipt.clone();
    receipt.retain_permit(Arc::new(
        cache
            .continuation_slots
            .clone()
            .try_acquire_owned()
            .unwrap(),
    ));
    first.publish_effects(make_response("EffectsReady"));
    first.start_reactions();
    drop(first);
    clock.advance_by((IDEMPOTENCY_TTL_SECS as u64 + 1) * 10);
    for i in 0..=IDEMPOTENCY_BUDGET_PER_ACTOR {
        cache.put("Order:o1", &format!("other-{i}"), B, make_response("Other"));
    }
    cache.put_historical("Order:o1", "key-1", "wrong-binding", make_response("Old"));
    assert!(matches!(
        cache.lookup("Order:o1", "key-1", "wrong-binding"),
        IdempotencyLookup::Mismatch
    ));
    assert!(matches!(
        claim(&cache, "Order:o1", "key-1"),
        OperationClaim::Join(_)
    ));
    assert_eq!(cache.continuation_slots.available_permits(), 63);
    let context = crate::request_context::AgentContext::system();
    assert_eq!(
        receipt
            .wait(false, false, &context, false)
            .await
            .response
            .state
            .status,
        "EffectsReady"
    );
    assert!(
        !receipt
            .wait(true, false, &context, false)
            .await
            .response
            .success
    );
}

#[tokio::test]
async fn reaction_phase_recovery_reregisters_existing_observers_after_coalesced_signals() {
    use super::wait_graph::Registration;
    use futures_util::poll;
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "parent", B, make_response("Running"));
    let OperationClaim::Owner(parent) = claim(&cache, "Order:o1", "parent") else {
        panic!("parent owner")
    };
    parent.publish_effects(make_response("Ready"));
    parent.start_reactions();
    cache.put("Order:o1", "key-1", B, make_response("Running"));
    let first = owner(&cache);
    let receipt = first.receipt.clone();
    first.publish_effects(make_response("Ready"));
    let context = crate::request_context::AgentContext {
        local_completion: crate::request_context::LocalCompletionEvidence::for_operation(
            &parent.receipt,
        ),
        ..crate::request_context::AgentContext::system()
    };
    let mut observer = Box::pin(receipt.wait(true, false, &context, false));
    assert!(poll!(observer.as_mut()).is_pending());
    drop(first);
    let resumed = owner(&cache);
    // The observer never polls the intervening recoverable failure snapshot.
    assert!(poll!(observer.as_mut()).is_pending());
    resumed.start_reactions();
    assert!(
        matches!(
            cache
                .wait_graph
                .register(&resumed.receipt, &parent.receipt, true),
            Registration::ReactionBackEdge
        ),
        "coalesced phase signals lost the observer's real dependency"
    );
}
