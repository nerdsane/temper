//! DST persistence tests: Real EntityActor + SimEventStore.
//!
//! These tests verify that the real persistence code path (EntityActor with
//! ServerEventStore) works correctly with the in-memory SimEventStore backend.
//! All tests run across multiple seeds to catch timing-dependent bugs.
//!
//! FoundationDB pattern: same code, simulated I/O.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::{EntityActor, EntityMsg, EntityResponse, ServerEventStore};
use temper_store_sim::SimEventStore;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const NUM_SEEDS: u64 = 100;

fn order_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA)))
}

fn sim_store(seed: u64) -> Arc<ServerEventStore> {
    Arc::new(ServerEventStore::Sim(SimEventStore::no_faults(seed)))
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
            },
            Duration::from_secs(5),
        )
        .await
        .expect("actor should respond")
}

async fn get_state(actor_ref: &temper_runtime::actor::ActorRef<EntityMsg>) -> EntityResponse {
    actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("actor should respond")
}

// =========================================================================
// Test: Replay fidelity — create, advance, crash, replay, verify
// =========================================================================

#[tokio::test]
async fn dst_replay_fidelity() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let store = sim_store(seed);
        let table = order_table();
        let entity_id = format!("ord-{seed}");

        // Phase 1: Create entity, run actions, capture state.
        let (pre_crash_status, pre_crash_event_count) = {
            let system = ActorSystem::new("dst-replay");
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                serde_json::json!({}),
                store.clone(),
            )
            .with_tenant("default");
            let actor_ref = system.spawn(actor, &entity_id);

            let r = dispatch_action(&actor_ref, "AddItem", serde_json::json!({})).await;
            assert!(r.success, "seed {seed}: AddItem failed: {:?}", r.error);

            let r = dispatch_action(&actor_ref, "SubmitOrder", serde_json::json!({})).await;
            assert!(r.success, "seed {seed}: SubmitOrder failed: {:?}", r.error);

            let r = dispatch_action(&actor_ref, "ConfirmOrder", serde_json::json!({})).await;
            assert!(r.success, "seed {seed}: ConfirmOrder failed: {:?}", r.error);

            let pre = get_state(&actor_ref).await;
            (pre.state.status.clone(), pre.state.events.len())
        };
        // actor_ref + system dropped — simulates crash.

        // Phase 2: Respawn with same store, verify state replay.
        let (_guard2, _clock2, _id_gen2) = install_deterministic_context(seed + 1000);
        let system2 = ActorSystem::new("dst-replay-2");
        let actor2 = EntityActor::with_persistence(
            "Order",
            &entity_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
        )
        .with_tenant("default");
        let actor_ref2 = system2.spawn(actor2, format!("{entity_id}-replay"));

        let post = get_state(&actor_ref2).await;
        assert_eq!(
            post.state.status, pre_crash_status,
            "seed {seed}: status mismatch after replay"
        );
        assert_eq!(
            post.state.events.len(),
            pre_crash_event_count,
            "seed {seed}: event count mismatch after replay"
        );
    }
}

// =========================================================================
// Test: Sequence monotonicity
// =========================================================================

#[tokio::test]
async fn dst_sequence_monotonicity() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let store_inner = SimEventStore::no_faults(seed);
        let store = Arc::new(ServerEventStore::Sim(store_inner.clone()));
        let table = order_table();
        let system = ActorSystem::new("dst-seq");

        let entity_id = format!("ord-seq-{seed}");
        let actor = EntityActor::with_persistence(
            "Order",
            &entity_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
        )
        .with_tenant("default");
        let actor_ref = system.spawn(actor, &entity_id);

        let actions = ["AddItem", "SubmitOrder", "ConfirmOrder", "ProcessOrder"];
        for action in &actions {
            let r = dispatch_action(&actor_ref, action, serde_json::json!({})).await;
            assert!(r.success, "seed {seed}: {action} failed: {:?}", r.error);
        }

        // Verify sequence numbers are strictly monotonic.
        let persistence_id = format!("default:Order:{entity_id}");
        let events = store_inner.dump_journal(&persistence_id);
        assert!(!events.is_empty(), "seed {seed}: no events persisted");

        for i in 1..events.len() {
            assert!(
                events[i].sequence_nr > events[i - 1].sequence_nr,
                "seed {seed}: sequence not monotonic at index {i}: {} <= {}",
                events[i].sequence_nr,
                events[i - 1].sequence_nr
            );
        }
    }
}

// =========================================================================
// Test: Crash recovery — advance, crash, respawn, continue
// =========================================================================

#[tokio::test]
async fn dst_crash_recovery() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let store = sim_store(seed);
        let table = order_table();
        let entity_id = format!("ord-crash-{seed}");

        // Phase 1: Create and advance.
        {
            let system = ActorSystem::new("dst-crash-1");
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                serde_json::json!({}),
                store.clone(),
            )
            .with_tenant("default");
            let actor_ref = system.spawn(actor, &entity_id);

            dispatch_action(&actor_ref, "AddItem", serde_json::json!({})).await;
            dispatch_action(&actor_ref, "SubmitOrder", serde_json::json!({})).await;

            let state = get_state(&actor_ref).await;
            assert_eq!(state.state.status, "Submitted", "seed {seed}");
        }

        // Phase 2: Respawn and continue.
        {
            let (_guard2, _clock2, _id_gen2) = install_deterministic_context(seed + 5000);
            let system = ActorSystem::new("dst-crash-2");
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                serde_json::json!({}),
                store.clone(),
            )
            .with_tenant("default");
            let actor_ref = system.spawn(actor, format!("{entity_id}-2"));

            let state = get_state(&actor_ref).await;
            assert_eq!(
                state.state.status, "Submitted",
                "seed {seed}: status not restored"
            );

            let r = dispatch_action(&actor_ref, "ConfirmOrder", serde_json::json!({})).await;
            assert!(r.success, "seed {seed}: ConfirmOrder failed: {:?}", r.error);
            assert_eq!(r.state.status, "Confirmed");
        }
    }
}

// =========================================================================
// Test: Determinism canary — same seed produces identical state
// =========================================================================

#[tokio::test]
async fn dst_determinism_canary() {
    for seed in 0..50 {
        let mut results = Vec::new();

        for run in 0..2 {
            let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
            let store_inner = SimEventStore::no_faults(seed);
            let store = Arc::new(ServerEventStore::Sim(store_inner.clone()));
            let table = order_table();
            let system = ActorSystem::new(format!("dst-det-{run}"));

            let entity_id = format!("ord-det-{seed}");
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                serde_json::json!({}),
                store.clone(),
            )
            .with_tenant("default");
            let actor_ref = system.spawn(actor, &entity_id);

            for action in &["AddItem", "SubmitOrder", "ConfirmOrder"] {
                dispatch_action(&actor_ref, action, serde_json::json!({})).await;
            }

            let state = get_state(&actor_ref).await;
            results.push((
                state.state.status.clone(),
                state.state.events.len(),
                state.state.sequence_nr,
            ));
        }

        assert_eq!(results[0], results[1], "seed {seed}: determinism violation");
    }
}

// =========================================================================
// Test: In-memory entity with no persistence works as before
// =========================================================================

#[tokio::test]
async fn dst_in_memory_entity_unaffected() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let table = order_table();
    let system = ActorSystem::new("dst-inmem");

    let actor = EntityActor::new("Order", "ord-inmem", table, serde_json::json!({}));
    let actor_ref = system.spawn(actor, "ord-inmem");

    let r = dispatch_action(&actor_ref, "AddItem", serde_json::json!({})).await;
    assert!(r.success);

    let r = dispatch_action(&actor_ref, "SubmitOrder", serde_json::json!({})).await;
    assert!(r.success);
    assert_eq!(r.state.status, "Submitted");
}
