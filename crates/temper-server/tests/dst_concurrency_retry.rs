//! DST tests for the ADR-0046 optimistic-concurrency retry path.
//!
//! Uses `SimEventStore::inject_concurrency_violations` to deterministically
//! queue `ConcurrencyViolation` errors on specific append calls, then verifies
//! the `EntityActor` retry loop behaves correctly:
//!
//! 1. One injected violation → retry absorbs it, action succeeds.
//! 2. Violations for every attempt → retry budget exhausts cleanly and the
//!    caller sees a distinct error.
//!
//! These tests cover ADR-0046 Rollout Phase 0 "DST race test" follow-up.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::{EntityActor, EntityMsg, EntityResponse};
use temper_store_sim::SimEventStore;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn order_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA)))
}

fn sim_store_with_handle(seed: u64) -> (BoxedEventStore, SimEventStore) {
    let inner = SimEventStore::no_faults(seed);
    let store = BoxedEventStore::new(inner.clone());
    (store, inner)
}

async fn dispatch_action(
    actor_ref: &temper_runtime::actor::ActorRef<EntityMsg>,
    action: &str,
    params: serde_json::Value,
) -> EntityResponse {
    actor_ref
        .ask(
            EntityMsg::Action {
                name: action.to_string(),
                params,
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: None,
                expected_sequence: None,
                reaction_context: None,
                expected_authorization_precondition: None,
                kernel_metadata: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("actor should respond")
}

async fn dispatch_action_with_preconditions(
    actor_ref: &temper_runtime::actor::ActorRef<EntityMsg>,
    expected_sequence: u64,
    idempotency_key: &str,
) -> EntityResponse {
    actor_ref
        .ask(
            EntityMsg::Action {
                name: "AddItem".into(),
                params: add_item_params(),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: Some(idempotency_key.into()),
                expected_sequence: Some(expected_sequence),
                reaction_context: None,
                expected_authorization_precondition: None,
                kernel_metadata: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("actor should respond")
}

fn add_item_params() -> serde_json::Value {
    serde_json::json!({"ProductId": "product-1", "Quantity": 1})
}

/// Wait for the actor's `pre_start` to complete (which writes the bootstrap
/// `Create` event to the journal) before injecting retry faults on later
/// appends. Using `GetState` as the synchronisation point — it only replies
/// after `pre_start` returns, so after the await we are guaranteed the
/// `Create` append is in the journal and the injection counter will target
/// the next action dispatch instead.
async fn wait_ready(actor_ref: &temper_runtime::actor::ActorRef<EntityMsg>) {
    let _: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("actor should respond");
}

/// Harness: spawn an `Order` actor bound to the given persistence id against a
/// `SimEventStore`-backed StorageStack journal. Returns the actor ref plus the
/// raw sim handle so tests can queue violations and inspect the journal.
fn spawn_order(
    system: &ActorSystem,
    table: Arc<RwLock<TransitionTable>>,
    store: BoxedEventStore,
    entity_id: &str,
) -> temper_runtime::actor::ActorRef<EntityMsg> {
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    system.spawn(actor, entity_id)
}

// -------------------------------------------------------------------------
// Test 1: Success after one injected concurrency violation.
// -------------------------------------------------------------------------
//
// Queues a single `ConcurrencyViolation` for the entity's persistence id, then
// dispatches `AddItem`. The first `persist_event` call hits the injected
// violation, the retry loop rolls back, replays (journal is empty so state
// stays at sequence 0), re-evaluates `AddItem`, sleeps 10ms, and retries the
// persist. Second attempt succeeds because the injection counter drained to
// zero on the first call.
#[tokio::test]
async fn dst_retry_succeeds_after_one_violation() {
    let seed = 7;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let (store, sim) = sim_store_with_handle(seed);
    let table = order_table();
    let entity_id = "ord-retry-1";
    let persistence_id = format!("default:Order:{entity_id}");

    let system = ActorSystem::new("dst-retry");
    let actor_ref = spawn_order(&system, table, store, entity_id);
    // Let pre_start finish (writes the bootstrap Create event) before
    // injecting the violation so the retry is exercised on AddItem, not Create.
    wait_ready(&actor_ref).await;
    sim.inject_concurrency_violations(&persistence_id, 1);

    let resp = dispatch_action(&actor_ref, "AddItem", add_item_params()).await;

    assert!(
        resp.success,
        "retry should absorb one violation and commit; got: {:?}",
        resp.error
    );
    assert_eq!(resp.state.status, "Draft");
    assert_eq!(resp.state.item_count, 1);

    // Injection counter must be drained after the retry consumed it.
    assert_eq!(
        sim.pending_concurrency_violations(&persistence_id),
        0,
        "injection should be consumed"
    );

    // Journal: Create (from pre_start) + AddItem (retry succeeded) = 2 events.
    // The first AddItem attempt hit the injection and never touched the journal.
    assert_eq!(sim.dump_journal(&persistence_id).len(), 2);
}

#[tokio::test]
async fn exact_sequence_does_not_retry_after_persistence_conflict() {
    let seed = 19;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let (store, sim) = sim_store_with_handle(seed);
    let entity_id = "ord-exact-sequence";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-exact-sequence");
    let actor_ref = spawn_order(&system, order_table(), store, entity_id);
    wait_ready(&actor_ref).await;
    sim.inject_concurrency_violations(&persistence_id, 1);

    let response = dispatch_action_with_preconditions(&actor_ref, 1, "exact-1").await;
    assert!(!response.success);
    assert_eq!(response.error.as_deref(), Some("SequenceConflict"));
    assert_eq!(sim.dump_journal(&persistence_id).len(), 1);
}

#[tokio::test]
async fn idempotent_retry_precedes_stale_sequence_rejection() {
    let seed = 23;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let (store, sim) = sim_store_with_handle(seed);
    let entity_id = "ord-idempotent-sequence";
    let persistence_id = format!("default:Order:{entity_id}");
    let system = ActorSystem::new("dst-idempotent-sequence");
    let actor_ref = spawn_order(&system, order_table(), store, entity_id);
    wait_ready(&actor_ref).await;

    let first = dispatch_action_with_preconditions(&actor_ref, 1, "same-key").await;
    assert!(first.success);
    let retry = dispatch_action_with_preconditions(&actor_ref, 1, "same-key").await;
    assert!(retry.success);
    assert_eq!(sim.dump_journal(&persistence_id).len(), 2);
}

// -------------------------------------------------------------------------
// Test 2: Retry budget exhausts cleanly under sustained violation.
// -------------------------------------------------------------------------
//
// Queues enough violations to cover every attempt (3 total). The retry loop
// consumes all attempts, surfaces "optimistic concurrency retry exhausted",
// and rolls back speculative state. No event lands in the journal.
#[tokio::test]
async fn dst_retry_exhausts_under_sustained_violation() {
    let seed = 11;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let (store, sim) = sim_store_with_handle(seed);
    let table = order_table();
    let entity_id = "ord-retry-2";
    let persistence_id = format!("default:Order:{entity_id}");

    let system = ActorSystem::new("dst-retry");
    let actor_ref = spawn_order(&system, table, store, entity_id);
    wait_ready(&actor_ref).await;

    // 1 initial + 2 retries = 3 total attempts; queue one more than that to
    // prove the caller sees a distinct error even if extra failures are queued
    // (the retry budget is the tight bound, not injection exhaustion).
    sim.inject_concurrency_violations(&persistence_id, 4);

    let resp = dispatch_action(&actor_ref, "AddItem", add_item_params()).await;

    assert!(
        !resp.success,
        "3 attempts all hitting ConcurrencyViolation must fail the caller"
    );
    let err = resp
        .error
        .clone()
        .expect("failure path populates the error field");
    assert!(
        err.contains("optimistic concurrency retry exhausted")
            || err.contains("persistence failed"),
        "error must identify the retry-exhaustion failure mode, got: {err}"
    );

    // Speculative state was rolled back — item count stays at the
    // pre-action baseline from post-Create state.
    assert_eq!(resp.state.item_count, 0);

    // Exactly one injection counter unit remains (4 queued, 3 consumed).
    assert_eq!(
        sim.pending_concurrency_violations(&persistence_id),
        1,
        "attempt budget caps consumption at 3, not injection drain"
    );

    // Journal still holds just the bootstrap Create — the failed AddItem
    // attempts never touched it.
    assert_eq!(
        sim.dump_journal(&persistence_id).len(),
        1,
        "only the pre-retry Create should be in the journal"
    );
}

// -------------------------------------------------------------------------
// Test 3: Multi-seed robustness on the success path.
// -------------------------------------------------------------------------
//
// The single-violation success path should hold across many seeds. This
// catches any hidden wall-clock ordering assumption and is the DST "race
// coverage" ask from ADR-0046 Rollout Phase 0.
#[tokio::test]
async fn dst_retry_succeeds_after_one_violation_many_seeds() {
    for seed in 0..25u64 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (store, sim) = sim_store_with_handle(seed);
        let table = order_table();
        let entity_id = format!("ord-seed-{seed}");
        let persistence_id = format!("default:Order:{entity_id}");

        let system = ActorSystem::new("dst-retry-seeds");
        let actor_ref = spawn_order(&system, table, store, &entity_id);
        wait_ready(&actor_ref).await;
        sim.inject_concurrency_violations(&persistence_id, 1);

        let resp = dispatch_action(&actor_ref, "AddItem", add_item_params()).await;

        assert!(
            resp.success,
            "seed {seed}: retry should absorb one violation, got: {:?}",
            resp.error
        );
        assert_eq!(resp.state.item_count, 1);
        assert_eq!(
            sim.dump_journal(&persistence_id).len(),
            2,
            "seed {seed}: journal = Create + successful AddItem"
        );
    }
}
