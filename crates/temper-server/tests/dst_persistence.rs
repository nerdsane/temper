//! DST persistence tests: Real EntityActor + SimEventStore.
//!
//! These tests verify that the real persistence code path (EntityActor with
//! StorageStack event journal) works correctly with the in-memory
//! SimEventStore backend.
//! All tests run across multiple seeds to catch timing-dependent bugs.
//!
//! FoundationDB pattern: same code, simulated I/O.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_server::key_index::canonical_key_hash;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::{EntityActor, EntityMsg, EntityResponse};
use temper_store_sim::{SimEventStore, SimFaultConfig};

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const NUM_SEEDS: u64 = 100;

fn order_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA)))
}

fn sim_store(seed: u64) -> BoxedEventStore {
    BoxedEventStore::new(SimEventStore::no_faults(seed))
}

fn order_key_hash(workspace: &str, path: &str) -> String {
    canonical_key_hash(
        "ws_path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        &serde_json::Map::from_iter([
            ("WorkspaceId".to_string(), serde_json::json!(workspace)),
            ("Path".to_string(), serde_json::json!(path)),
        ]),
    )
    .expect("complete order key")
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

async fn update_fields(
    actor_ref: &temper_runtime::actor::ActorRef<EntityMsg>,
    fields: serde_json::Value,
    replace: bool,
) -> EntityResponse {
    actor_ref
        .ask(
            EntityMsg::UpdateFields { fields, replace },
            Duration::from_secs(5),
        )
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
                BackendLabel::Sim,
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
            BackendLabel::Sim,
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
        let store = BoxedEventStore::new(store_inner.clone());
        let table = order_table();
        let system = ActorSystem::new("dst-seq");

        let entity_id = format!("ord-seq-{seed}");
        let actor = EntityActor::with_persistence(
            "Order",
            &entity_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
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
                BackendLabel::Sim,
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
                BackendLabel::Sim,
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
            let store = BoxedEventStore::new(store_inner.clone());
            let table = order_table();
            let system = ActorSystem::new(format!("dst-det-{run}"));

            let entity_id = format!("ord-det-{seed}");
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                serde_json::json!({}),
                store.clone(),
                BackendLabel::Sim,
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

// =========================================================================
// Test: Data fields (action params) survive replay
// =========================================================================

#[tokio::test]
async fn dst_replay_preserves_data_fields() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let store = sim_store(seed);
        let table = order_table();
        let entity_id = format!("ord-fields-{seed}");

        // Phase 1: Create entity with data fields in action params.
        let pre_crash_fields = {
            let system = ActorSystem::new("dst-fields-1");
            let initial = serde_json::json!({"Title": "Test Order", "CustomerId": "cust-42"});
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                initial,
                store.clone(),
                BackendLabel::Sim,
            )
            .with_tenant("default");
            let actor_ref = system.spawn(actor, &entity_id);

            // AddItem with ProductId param — this is a data field.
            let r = dispatch_action(
                &actor_ref,
                "AddItem",
                serde_json::json!({"ProductId": "prod-99", "Quantity": "3"}),
            )
            .await;
            assert!(r.success, "seed {seed}: AddItem failed: {:?}", r.error);

            // SubmitOrder with more data fields.
            let r = dispatch_action(
                &actor_ref,
                "SubmitOrder",
                serde_json::json!({"ShippingAddressId": "addr-1", "PaymentMethod": "credit"}),
            )
            .await;
            assert!(r.success, "seed {seed}: SubmitOrder failed: {:?}", r.error);

            let state = get_state(&actor_ref).await;
            state.state.fields.clone()
        };
        // Actor dropped — simulates crash.

        // Phase 2: Respawn with same store, verify data fields survive.
        let (_guard2, _clock2, _id_gen2) = install_deterministic_context(seed + 2000);
        let system2 = ActorSystem::new("dst-fields-2");
        let actor2 = EntityActor::with_persistence(
            "Order",
            &entity_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let actor_ref2 = system2.spawn(actor2, format!("{entity_id}-replay"));

        let post = get_state(&actor_ref2).await;
        let post_fields = &post.state.fields;

        // Verify initial fields from creation survive.
        assert_eq!(
            post_fields.get("Title").and_then(|v| v.as_str()),
            Some("Test Order"),
            "seed {seed}: Title lost after replay"
        );
        assert_eq!(
            post_fields.get("CustomerId").and_then(|v| v.as_str()),
            Some("cust-42"),
            "seed {seed}: CustomerId lost after replay"
        );
        // Verify action params survive.
        assert_eq!(
            post_fields.get("ProductId").and_then(|v| v.as_str()),
            Some("prod-99"),
            "seed {seed}: ProductId lost after replay"
        );
        assert_eq!(
            post_fields
                .get("ShippingAddressId")
                .and_then(|v| v.as_str()),
            Some("addr-1"),
            "seed {seed}: ShippingAddressId lost after replay"
        );
        assert_eq!(
            post_fields.get("PaymentMethod").and_then(|v| v.as_str()),
            Some("credit"),
            "seed {seed}: PaymentMethod lost after replay"
        );

        // Verify all fields match pre-crash state.
        assert_eq!(
            pre_crash_fields, post.state.fields,
            "seed {seed}: fields mismatch after replay"
        );
    }
}

// =========================================================================
// Regression: PATCH-only fields survive actor replacement
// =========================================================================

#[tokio::test]
async fn dst_patch_only_fields_survive_actor_replacement() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let store = sim_store(seed);
        let table = order_table();
        let entity_id = format!("ord-patch-only-{seed}");

        {
            let system = ActorSystem::new("dst-patch-only-1");
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                serde_json::json!({
                    "Title": "before-patch",
                    "StableField": "preserved",
                    "WorkspaceId": "ws-patch",
                    "Path": "/before"
                }),
                store.clone(),
                BackendLabel::Sim,
            )
            .with_tenant("default");
            let actor_ref = system.spawn(actor, &entity_id);

            let patched = update_fields(
                &actor_ref,
                serde_json::json!({
                    "Title": "after-patch",
                    "Priority": "High",
                    "Path": "/after"
                }),
                false,
            )
            .await;
            assert!(
                patched.success,
                "seed {seed}: PATCH failed: {:?}",
                patched.error
            );
            assert_eq!(patched.state.fields["Title"], "after-patch");
            assert_eq!(patched.state.fields["Priority"], "High");
            assert_eq!(
                store
                    .lookup_by_key(
                        "default",
                        "Order",
                        "ws_path",
                        &order_key_hash("ws-patch", "/before"),
                    )
                    .await
                    .expect("old key lookup should succeed"),
                None,
                "seed {seed}: old declared-key projection was not removed"
            );
            assert_eq!(
                store
                    .lookup_by_key(
                        "default",
                        "Order",
                        "ws_path",
                        &order_key_hash("ws-patch", "/after"),
                    )
                    .await
                    .expect("new key lookup should succeed"),
                Some(entity_id.clone()),
                "seed {seed}: PATCH did not co-commit its declared-key projection"
            );
        }

        {
            let (_guard2, _clock2, _id_gen2) = install_deterministic_context(seed + 10_000);
            let system = ActorSystem::new("dst-patch-only-2");
            let actor = EntityActor::with_persistence(
                "Order",
                &entity_id,
                table.clone(),
                serde_json::json!({}),
                store.clone(),
                BackendLabel::Sim,
            )
            .with_tenant("default");
            let actor_ref = system.spawn(actor, format!("{entity_id}-replacement"));

            let replayed = get_state(&actor_ref).await;
            assert_eq!(
                replayed.state.fields["Title"], "after-patch",
                "seed {seed}: acknowledged PATCH was lost after actor replacement"
            );
            assert_eq!(
                replayed.state.fields["Priority"], "High",
                "seed {seed}: PATCH-only field was lost after actor replacement"
            );
            assert_eq!(replayed.state.fields["StableField"], "preserved");

            let replaced =
                update_fields(&actor_ref, serde_json::json!({"Title": "after-put"}), true).await;
            assert!(replaced.success, "seed {seed}: PUT failed");
            assert!(replaced.state.fields.get("Priority").is_none());
            assert!(replaced.state.fields.get("StableField").is_none());
            assert_eq!(
                store
                    .lookup_by_key(
                        "default",
                        "Order",
                        "ws_path",
                        &order_key_hash("ws-patch", "/after"),
                    )
                    .await
                    .expect("removed key lookup should succeed"),
                None,
                "seed {seed}: PUT left a stale declared-key projection"
            );
        }

        let (_guard3, _clock3, _id_gen3) = install_deterministic_context(seed + 20_000);
        let system = ActorSystem::new("dst-patch-only-3");
        let actor = EntityActor::with_persistence(
            "Order",
            &entity_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let actor_ref = system.spawn(actor, format!("{entity_id}-put-replacement"));
        let replayed = get_state(&actor_ref).await;
        assert_eq!(replayed.state.fields["Title"], "after-put");
        assert!(
            replayed.state.fields.get("Priority").is_none(),
            "seed {seed}: PUT replay resurrected a removed PATCH field"
        );
        assert!(replayed.state.fields.get("StableField").is_none());
        assert_eq!(
            store
                .lookup_by_key(
                    "default",
                    "Order",
                    "ws_path",
                    &order_key_hash("ws-patch", "/after"),
                )
                .await
                .expect("replayed removed key lookup should succeed"),
            None,
            "seed {seed}: stale key returned after PUT replay"
        );
    }
}

// =========================================================================
// Regression: a failed journal append cannot publish a field update
// =========================================================================

#[tokio::test]
async fn dst_field_update_fails_closed_when_journal_append_fails() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(189);
    let store_inner = SimEventStore::no_faults(189);
    let store = BoxedEventStore::new(store_inner.clone());
    let table = order_table();
    let system = ActorSystem::new("dst-field-update-fail-closed");
    let actor = EntityActor::with_persistence(
        "Order",
        "ord-field-failure",
        table,
        serde_json::json!({"Title": "durable-before"}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, "ord-field-failure");

    let before = get_state(&actor_ref).await;
    assert_eq!(before.state.fields["Title"], "durable-before");
    let sequence_before = before.state.sequence_nr;

    store_inner.restore_faults(SimFaultConfig {
        write_failure_prob: 1.0,
        ..SimFaultConfig::none()
    });

    let response = update_fields(
        &actor_ref,
        serde_json::json!({"Title": "volatile-after"}),
        false,
    )
    .await;

    assert!(
        !response.success,
        "field update must not report success when its journal append fails"
    );
    assert_eq!(response.state.fields["Title"], "durable-before");
    assert_eq!(response.state.sequence_nr, sequence_before);

    let live = get_state(&actor_ref).await;
    assert_eq!(live.state.fields["Title"], "durable-before");
    assert_eq!(live.state.sequence_nr, sequence_before);
}

#[tokio::test]
async fn dst_field_update_retries_after_concurrency_violation() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(18_900);
    let store_inner = SimEventStore::no_faults(18_900);
    let store = BoxedEventStore::new(store_inner.clone());
    let table = order_table();
    let entity_id = "ord-field-concurrency";
    let persistence_id = format!("default:Order:{entity_id}");

    {
        let system = ActorSystem::new("dst-field-update-concurrency-1");
        let actor = EntityActor::with_persistence(
            "Order",
            entity_id,
            table.clone(),
            serde_json::json!({"Title": "before-retry"}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let actor_ref = system.spawn(actor, entity_id);
        let before = get_state(&actor_ref).await;
        assert_eq!(before.state.sequence_nr, 1);

        store_inner
            .append(
                &persistence_id,
                1,
                &[PersistenceEnvelope {
                    sequence_nr: 2,
                    event_type: "AddItem".to_string(),
                    payload: serde_json::json!({
                        "action": "AddItem",
                        "from_status": "Draft",
                        "to_status": "Draft",
                        "timestamp": sim_now(),
                        "params": {"ProductId": "concurrent-item"}
                    }),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: sim_now(),
                        actor_id: persistence_id.clone(),
                    },
                }],
            )
            .await
            .expect("concurrent action should advance authoritative history");
        let response = update_fields(
            &actor_ref,
            serde_json::json!({"Title": "after-retry"}),
            false,
        )
        .await;
        assert!(
            response.success,
            "field update retry failed: {:?}",
            response.error
        );
        assert_eq!(response.state.item_count, 1);
        assert_eq!(response.state.fields["ProductId"], "concurrent-item");
    }

    let system = ActorSystem::new("dst-field-update-concurrency-2");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table,
        serde_json::json!({}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, "ord-field-concurrency-replacement");
    let replayed = get_state(&actor_ref).await;
    assert_eq!(replayed.state.fields["Title"], "after-retry");
    assert_eq!(replayed.state.item_count, 1);
    assert_eq!(replayed.state.fields["ProductId"], "concurrent-item");
}

#[tokio::test]
async fn dst_reserved_field_update_event_type_cannot_be_dispatched_as_action() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(18_901);
    let store_inner = SimEventStore::no_faults(18_901);
    let store = BoxedEventStore::new(store_inner.clone());
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "ReservedAction"
states = ["Draft", "Updated"]
initial = "Draft"

[[action]]
name = "$temper.entity.fields-updated.v1"
kind = "input"
from = ["Draft"]
to = "Updated"
"#,
    )));
    let system = ActorSystem::new("dst-reserved-field-update-event");
    let actor = EntityActor::with_persistence(
        "ReservedAction",
        "reserved-action-1",
        table,
        serde_json::json!({}),
        store,
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, "reserved-action-1");

    let response = dispatch_action(
        &actor_ref,
        "$temper.entity.fields-updated.v1",
        serde_json::json!({}),
    )
    .await;
    assert!(
        !response.success,
        "reserved journal type must not run as an action"
    );
    assert_eq!(response.state.status, "Draft");
    let journal = store_inner.dump_journal("default:ReservedAction:reserved-action-1");
    assert_eq!(journal.len(), 1, "only the bootstrap event may be durable");
    assert_eq!(journal[0].event_type, "Created");
}
