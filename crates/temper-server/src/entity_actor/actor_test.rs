use super::*;
use std::collections::BTreeMap;
use std::time::Duration;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

fn order_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA)))
}

fn composite_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Repository"
version = "1.0.0"
states = ["Active"]
initial = "Active"

[[action]]
name = "IngestPack"
kind = "Composite"
from = ["Active"]
to = "Active"
params = ["PackBytes", "RefUpdates", "ClientRequestId"]
effect = [{ type = "trigger", name = "scm_ingest_pack" }]

[[action.sub_writes]]
target_entity = "Commit"
action = "Create"

[[integration]]
name = "scm_ingest_pack"
trigger = "scm_ingest_pack"
type = "wasm"
module = "scm_ingest_pack"
"#,
    )))
}

const DELETE_INVARIANT_IOA: &str = r#"
[automaton]
name = "Document"
states = ["Active", "Deleted"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "deletion_reason"
type = "string"
initial = ""

[[invariant]]
name = "DeletedRequiresReason"
when = ["Deleted"]
assert = "deletion_reason != ''"
"#;

#[test]
fn event_budget_workspace_id_uses_workspace_entity_id_or_field() {
    let workspace_state = EntityState {
        entity_type: "Workspace".to_string(),
        entity_id: "ws-1".to_string(),
        status: "Active".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({"WorkspaceId": "ignored"}),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    };
    assert_eq!(event_budget_workspace_id(&workspace_state), "ws-1");

    let file_state = EntityState {
        entity_type: "File".to_string(),
        entity_id: "fl-1".to_string(),
        status: "Ready".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({"workspace_id": "ws-2"}),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    };
    assert_eq!(event_budget_workspace_id(&file_state), "ws-2");
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn queued_snapshot_only_advances_replay_boundary_after_write_applies() {
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(43));
    let boxed_store = crate::storage::BoxedEventStore::from_arc(store);
    let snapshot_queue = SnapshotWriteQueue::start(boxed_store.clone());
    let persistence_id = "default:Order:queued-snapshot-1";
    let table = TransitionTable::from_ioa_source(ORDER_IOA);
    let mut state = EntityState {
        entity_type: "Order".to_string(),
        entity_id: "queued-snapshot-1".to_string(),
        status: "Draft".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({
            "Id": "queued-snapshot-1",
            "Status": "Draft"
        }),
        events: std::collections::VecDeque::new(),
        total_event_count: 100,
        events_since_snapshot: 100,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 100,
        processed_idempotency_keys: BTreeMap::new(),
    };

    EntityActor::maybe_save_snapshot(
        &boxed_store,
        Some(&snapshot_queue),
        persistence_id,
        &mut state,
        &table,
    )
    .await
    .expect("snapshot enqueue should succeed");

    assert_eq!(snapshot_queue.pending_sequence(persistence_id), Some(100));
    assert_eq!(state.last_snapshot_sequence_nr, 0);
    assert_eq!(state.events_since_snapshot, 100);

    for _ in 0..20 {
        if snapshot_queue.applied_sequence(persistence_id) == Some(100) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(snapshot_queue.applied_sequence(persistence_id), Some(100));

    state.sequence_nr = 101;
    state.total_event_count = 101;
    state.events_since_snapshot = 101;
    EntityActor::maybe_save_snapshot(
        &boxed_store,
        Some(&snapshot_queue),
        persistence_id,
        &mut state,
        &table,
    )
    .await
    .expect("snapshot boundary observation should succeed");

    assert_eq!(state.last_snapshot_sequence_nr, 100);
    assert_eq!(state.events_since_snapshot, 1);
}

// =============================================
// DST-FIRST: Test the actor through the runtime
// =============================================

#[tokio::test]
async fn dst_entity_starts_in_initial_state() {
    let system = ActorSystem::new("dst");
    let table = order_table();
    let actor = EntityActor::new("Order", "order-1", table, serde_json::json!({}));
    let actor_ref = system.spawn(actor, "order-1");

    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .unwrap();

    assert_eq!(response.state.status, "Draft");
    assert_eq!(response.state.entity_id, "order-1");
    assert_eq!(response.state.item_count, 0);
    assert!(response.state.events.is_empty());
}

#[tokio::test]
async fn dst_add_item_then_submit() {
    let system = ActorSystem::new("dst");
    let table = order_table();
    let actor = EntityActor::new("Order", "order-2", table, serde_json::json!({}));
    let actor_ref = system.spawn(actor, "order-2");

    // Add an item (Draft -> Draft, item_count 0 -> 1)
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "AddItem".into(),
                params: serde_json::json!({"ProductId": "prod-1"}),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Draft");
    assert_eq!(r.state.item_count, 1);

    // Submit (Draft -> Submitted)
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "SubmitOrder".into(),
                params: serde_json::json!({"ShippingAddressId": "addr-1"}),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(r.success, "submit should succeed, got: {:?}", r.error);
    assert_eq!(r.state.status, "Submitted");
    assert_eq!(r.state.events.len(), 2); // AddItem + SubmitOrder
}

#[tokio::test]
async fn duplicate_composite_idempotency_reemits_spec_trigger() {
    let system = ActorSystem::new("composite-idempotency");
    let actor = EntityActor::new(
        "Repository",
        "rp-acme-app",
        composite_table(),
        serde_json::json!({}),
    );
    let actor_ref = system.spawn(actor, "rp-acme-app");
    let params = serde_json::json!({
        "PackBytes": "pack",
        "RefUpdates": [{"Name": "refs/heads/main"}],
        "ClientRequestId": "same-pack"
    });

    let first: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "IngestPack".into(),
                params: params.clone(),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: Some("same-pack".into()),
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_eq!(first.custom_effects, vec!["scm_ingest_pack"]);

    let duplicate: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "IngestPack".into(),
                params,
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: Some("same-pack".into()),
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();

    assert_eq!(duplicate.custom_effects, vec!["scm_ingest_pack"]);
    assert!(duplicate.state.fields.get("PackBytes").is_none());
    assert!(duplicate.state.fields.get("RefUpdates").is_none());
    assert!(duplicate.state.fields.get("ClientRequestId").is_none());
    assert_eq!(
        duplicate.state.events.len(),
        1,
        "duplicate idempotency must not append another parent event"
    );
}

#[tokio::test]
async fn dst_cannot_submit_without_items() {
    let system = ActorSystem::new("dst");
    let table = order_table();
    let actor = EntityActor::new("Order", "order-3", table, serde_json::json!({}));
    let actor_ref = system.spawn(actor, "order-3");

    // Try to submit with 0 items -- should fail
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "SubmitOrder".into(),
                params: serde_json::json!({}),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(!r.success);
    assert_eq!(r.state.status, "Draft"); // Still in Draft
}

#[tokio::test]
async fn dst_full_order_lifecycle() {
    let system = ActorSystem::new("dst");
    let table = order_table();
    let actor = EntityActor::new("Order", "order-4", table, serde_json::json!({}));
    let actor_ref = system.spawn(actor, "order-4");

    // Draft -> AddItem -> SubmitOrder -> ConfirmOrder -> ProcessOrder -> ShipOrder -> DeliverOrder
    let actions = [
        ("AddItem", serde_json::json!({})),
        ("SubmitOrder", serde_json::json!({})),
        ("ConfirmOrder", serde_json::json!({})),
        ("ProcessOrder", serde_json::json!({})),
        ("ShipOrder", serde_json::json!({})),
        ("DeliverOrder", serde_json::json!({})),
    ];

    let expected_states = [
        "Draft",      // after AddItem
        "Submitted",  // after SubmitOrder
        "Confirmed",  // after ConfirmOrder
        "Processing", // after ProcessOrder
        "Shipped",    // after ShipOrder
        "Delivered",  // after DeliverOrder
    ];

    for (i, (action, params)) in actions.into_iter().enumerate() {
        let r: EntityResponse = actor_ref
            .ask(
                EntityMsg::Action {
                    name: action.into(),
                    params,
                    cross_entity_booleans: std::collections::BTreeMap::new(),
                    idempotency_key: None,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(r.success, "step {i} ({action}) failed: {:?}", r.error);
        assert_eq!(
            r.state.status, expected_states[i],
            "step {i} ({action}) wrong state"
        );
    }

    // Verify full event log
    let r: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(r.state.events.len(), 6);
    assert_eq!(r.state.status, "Delivered");
}

#[tokio::test]
async fn dst_cancel_from_draft() {
    let system = ActorSystem::new("dst");
    let table = order_table();
    let actor = EntityActor::new("Order", "order-5", table, serde_json::json!({}));
    let actor_ref = system.spawn(actor, "order-5");

    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "CancelOrder".into(),
                params: serde_json::json!({"Reason": "changed mind"}),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Cancelled");
}

#[tokio::test]
async fn dst_cannot_cancel_shipped_order() {
    let system = ActorSystem::new("dst");
    let table = order_table();
    let actor = EntityActor::new("Order", "order-6", table, serde_json::json!({}));
    let actor_ref = system.spawn(actor, "order-6");

    // Drive to Shipped
    for action in &[
        "AddItem",
        "SubmitOrder",
        "ConfirmOrder",
        "ProcessOrder",
        "ShipOrder",
    ] {
        let _: EntityResponse = actor_ref
            .ask(
                EntityMsg::Action {
                    name: action.to_string(),
                    params: serde_json::json!({}),
                    cross_entity_booleans: std::collections::BTreeMap::new(),
                    idempotency_key: None,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
    }

    // Try to cancel -- should fail
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "CancelOrder".into(),
                params: serde_json::json!({}),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(!r.success);
    assert_eq!(r.state.status, "Shipped"); // Still Shipped
    assert!(r.error.unwrap().contains("not valid"));
}

#[tokio::test]
async fn dst_multiple_actors_independent() {
    let system = ActorSystem::new("dst");
    let table = order_table();

    let a1 = system.spawn(
        EntityActor::new("Order", "order-A", table.clone(), serde_json::json!({})),
        "order-A",
    );
    let a2 = system.spawn(
        EntityActor::new("Order", "order-B", table.clone(), serde_json::json!({})),
        "order-B",
    );

    // Cancel order A
    let _: EntityResponse = a1
        .ask(
            EntityMsg::Action {
                name: "CancelOrder".into(),
                params: serde_json::json!({}),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();

    // Add item to order B
    let _: EntityResponse = a2
        .ask(
            EntityMsg::Action {
                name: "AddItem".into(),
                params: serde_json::json!({}),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();

    // Verify independence
    let r1: EntityResponse = a1
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .unwrap();
    let r2: EntityResponse = a2
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .unwrap();

    assert_eq!(r1.state.status, "Cancelled");
    assert_eq!(r2.state.status, "Draft");
    assert_eq!(r2.state.item_count, 1);
}

/// Verify that replay skips events whose payload cannot be deserialized against
/// the current `EntityEvent` schema (schema evolution resilience).
///
/// The actor must reach a consistent final state using only the events that
/// parsed successfully, and must NOT panic on the schema-mismatched event.
#[cfg(feature = "sim")]
#[tokio::test]
async fn replay_skips_schema_mismatched_events() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(42));
    let pid = "default:Order:schema-evo-1";

    // Event 1: valid CancelOrder — parseable as EntityEvent.
    let good_env = PersistenceEnvelope {
        sequence_nr: 0, // overwritten by SimEventStore to 1
        event_type: "CancelOrder".to_string(),
        payload: serde_json::json!({
            "action": "CancelOrder",
            "from_status": "Draft",
            "to_status": "Cancelled",
            "timestamp": "2024-01-01T00:00:00Z",
            "params": {}
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };

    // Event 2: schema-mismatched — "action" is an integer, not a String.
    // Simulates a legacy event written under a previous schema version.
    let bad_env = PersistenceEnvelope {
        sequence_nr: 0, // overwritten by SimEventStore to 2
        event_type: "LegacyAction".to_string(),
        payload: serde_json::json!({
            "action": 999,
            "unknown_legacy_field": "leftover_from_old_schema"
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };

    store.append(pid, 0, &[good_env]).await.unwrap();
    store.append(pid, 1, &[bad_env]).await.unwrap();

    let system = ActorSystem::new("sim-replay-schema");
    let actor = EntityActor::with_persistence(
        "Order",
        "schema-evo-1",
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, "schema-evo-1");

    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();

    // Actor started cleanly despite the bad event.
    assert!(response.success);
    // The valid CancelOrder event was applied → status is Cancelled.
    assert_eq!(response.state.status, "Cancelled");
    // Both sequence numbers consumed (bad event's seq_nr was still advanced).
    assert_eq!(response.state.sequence_nr, 2);
    // Only the good event contributed to total_event_count.
    assert_eq!(response.state.total_event_count, 1);
}

/// Durable history that violates the current runtime safety contract must not
/// hydrate into a live entity. This covers both full journal replay and the
/// same validation applied after loading a snapshot plus its event tail.
#[cfg(feature = "sim")]
#[tokio::test]
async fn replay_rejects_persisted_runtime_invariant_violation() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Goal"
states = ["Draft", "Active"]
initial = "Draft"

[[state]]
name = "goal"
type = "string"
initial = ""

[[action]]
name = "Create"
kind = "input"
from = ["Draft"]
to = "Active"
params = ["goal"]

[[invariant]]
name = "active_goal_required"
when = ["Active"]
assert = "goal != ''"
"#,
    );
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:Goal:goal-replay-1";
    let envelope = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Create".to_string(),
        payload: serde_json::json!({
            "action": "Create",
            "from_status": "Draft",
            "to_status": "Active",
            "timestamp": "2024-01-01T00:00:00Z",
            "params": {"goal": ""}
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };
    store.append(pid, 0, &[envelope]).await.unwrap();
    let boxed_store = crate::storage::BoxedEventStore::from_arc(store);

    let error = recover_entity_state_from_store(
        "default",
        "Goal",
        "goal-replay-1",
        &table,
        &boxed_store,
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect_err("invalid durable history must fail hydration");

    assert!(
        error
            .to_string()
            .contains("persisted event violates runtime safety contract"),
        "unexpected replay error: {error}"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn declared_counter_action_param_survives_replay() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[state]]
name = "size_bytes"
type = "counter"
initial = "0"

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created", "Ready"]
to = "Ready"
params = ["size_bytes"]
"#,
    );
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:File:declared-counter-replay";
    store
        .append(
            pid,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "StreamUpdated".to_string(),
                payload: serde_json::json!({
                    "action": "StreamUpdated",
                    "from_status": "Created",
                    "to_status": "Ready",
                    "timestamp": "2024-01-01T00:00:00Z",
                    "params": {"size_bytes": 42}
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: pid.to_string(),
                },
            }],
        )
        .await
        .expect("append declared counter event");
    let boxed_store = crate::storage::BoxedEventStore::from_arc(store);

    let recovered = recover_entity_state_from_store(
        "default",
        "File",
        "declared-counter-replay",
        &table,
        &boxed_store,
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("declared counter event must replay");

    assert_eq!(recovered.counters["size_bytes"], 42);
    assert_eq!(recovered.fields["size_bytes"], 42);
    assert_eq!(recovered.status, "Ready");
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn modeled_effect_can_consume_protected_action_param_during_replay() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Cart"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "items"
type = "counter"
initial = "1"

[[action]]
name = "Update"
kind = "input"
from = ["Active"]
to = "Active"
params = ["items"]
effect = [{ type = "set_counter_from_param", var = "items", param = "items" }]

[[invariant]]
name = "HasItems"
when = ["Active"]
assert = "items > 0"
"#,
    );
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:Cart:protected-replay";
    store
        .append(
            pid,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Update".to_string(),
                payload: serde_json::json!({
                    "action": "Update",
                    "from_status": "Active",
                    "to_status": "Active",
                    "timestamp": "2024-01-01T00:00:00Z",
                    "params": {"items": 2}
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: pid.to_string(),
                },
            }],
        )
        .await
        .expect("append protected event");
    let boxed_store = crate::storage::BoxedEventStore::from_arc(store);

    let recovered = recover_entity_state_from_store(
        "default",
        "Cart",
        "protected-replay",
        &table,
        &boxed_store,
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("modeled effect must replay protected state");

    assert_eq!(recovered.counters["items"], 2);
    assert_eq!(recovered.fields["items"], 2);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn hydration_rejects_invalid_snapshot_before_healing_tail_event() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let source = r#"
[automaton]
name = "Goal"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "goal"
type = "string"
initial = ""

[[action]]
name = "Update"
kind = "input"
from = ["Active"]
to = "Active"
params = ["goal"]

[[invariant]]
name = "GoalRequired"
when = ["Active"]
assert = "goal != ''"
"#;
    let table = TransitionTable::from_ioa_source(source);
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:Goal:goal-snapshot-1";
    let event = |action: &str, params: serde_json::Value| PersistenceEnvelope {
        sequence_nr: 0,
        event_type: action.to_string(),
        payload: serde_json::json!({
            "action": action,
            "from_status": "Active",
            "to_status": "Active",
            "timestamp": "2024-01-01T00:00:00Z",
            "params": params
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };
    store
        .append(pid, 0, &[event("Created", serde_json::json!({}))])
        .await
        .unwrap();
    store
        .append(
            pid,
            1,
            &[event("Update", serde_json::json!({"goal": "healed"}))],
        )
        .await
        .unwrap();

    let mut invalid_snapshot = crate::entity_actor::effects::build_initial_entity_state(
        "Goal",
        "goal-snapshot-1",
        &table,
        &serde_json::json!({}),
    )
    .expect("build snapshot state");
    invalid_snapshot.sequence_nr = 1;
    invalid_snapshot.total_event_count = 1;
    store
        .save_snapshot(
            pid,
            1,
            &serde_json::to_vec(&invalid_snapshot).expect("serialize invalid snapshot"),
        )
        .await
        .unwrap();

    let error = recover_entity_state_from_store(
        "default",
        "Goal",
        "goal-snapshot-1",
        &table,
        &crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect_err("invalid snapshot must fail before a healing tail event");
    assert!(
        error
            .to_string()
            .contains("persisted snapshot violates runtime safety contract"),
        "unexpected snapshot error: {error}"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn model_protected_snapshot_is_rebuilt_from_authoritative_history() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Payment"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "payment_captured"
type = "bool"
initial = "true"

[[invariant]]
name = "PaymentCaptured"
when = ["Active"]
assert = "payment_captured"
"#,
    );
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:Payment:protected-snapshot";
    let mut corrupted = crate::entity_actor::effects::build_initial_entity_state(
        "Payment",
        "protected-snapshot",
        &table,
        &serde_json::json!({}),
    )
    .expect("build snapshot state");
    corrupted.booleans.insert("payment_captured".into(), false);
    corrupted.fields["payment_captured"] = serde_json::json!(false);
    corrupted.sequence_nr = 1;
    corrupted.total_event_count = 1;
    store
        .save_snapshot(
            pid,
            1,
            &serde_json::to_vec(&corrupted).expect("serialize corrupted snapshot"),
        )
        .await
        .expect("save corrupted snapshot");

    let recovered = recover_entity_state_from_store(
        "default",
        "Payment",
        "protected-snapshot",
        &table,
        &crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("protected state must rebuild without trusting snapshot values");

    assert!(recovered.booleans["payment_captured"]);
    assert_eq!(recovered.fields["payment_captured"], true);
    assert_eq!(recovered.sequence_nr, 0);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn contracted_protected_snapshot_remains_a_bounded_replay_boundary() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Payment"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "payment_captured"
type = "bool"
initial = "true"

[[invariant]]
name = "PaymentCaptured"
when = ["Active"]
assert = "payment_captured"
"#,
    );
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:Payment:protected-snapshot-boundary";
    let mut snapshot = crate::entity_actor::effects::build_initial_entity_state(
        "Payment",
        "protected-snapshot-boundary",
        &table,
        &serde_json::json!({}),
    )
    .expect("build snapshot state");
    snapshot.sequence_nr = (MAX_EVENTS_SINCE_SNAPSHOT as u64) + 1;
    snapshot.total_event_count = MAX_EVENTS_SINCE_SNAPSHOT + 1;
    let snapshot_bytes =
        EntityActor::serialize_snapshot_state(&snapshot, &table).expect("snapshot");
    store
        .save_snapshot(pid, snapshot.sequence_nr, &snapshot_bytes)
        .await
        .expect("save contracted snapshot");

    let recovered = recover_entity_state_from_store(
        "default",
        "Payment",
        "protected-snapshot-boundary",
        &table,
        &crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("contracted snapshot must avoid unbounded full replay");

    assert_eq!(recovered.sequence_nr, snapshot.sequence_nr);
    assert!(recovered.booleans["payment_captured"]);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn contracted_snapshot_sequence_mismatch_falls_back_to_full_replay() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Payment"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "payment_captured"
type = "bool"
initial = "true"

[[invariant]]
name = "PaymentCaptured"
when = ["Active"]
assert = "payment_captured"
"#,
    );
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:Payment:snapshot-sequence-mismatch";
    let mut snapshot = crate::entity_actor::effects::build_initial_entity_state(
        "Payment",
        "snapshot-sequence-mismatch",
        &table,
        &serde_json::json!({}),
    )
    .expect("build snapshot state");
    snapshot.sequence_nr = 5;
    snapshot.total_event_count = 5;
    let snapshot_bytes =
        EntityActor::serialize_snapshot_state(&snapshot, &table).expect("snapshot");
    store
        .save_snapshot(pid, 6, &snapshot_bytes)
        .await
        .expect("save mismatched snapshot row");

    let recovered = recover_entity_state_from_store(
        "default",
        "Payment",
        "snapshot-sequence-mismatch",
        &table,
        &crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("mismatched boundary must fall back to full history");

    assert_eq!(recovered.sequence_nr, 0);
    assert!(recovered.booleans["payment_captured"]);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn snapshot_from_weaker_safety_contract_is_not_a_replay_boundary() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let old_table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Payment"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "payment_captured"
type = "bool"
initial = "true"
"#,
    );
    let new_table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Payment"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "payment_captured"
type = "bool"
initial = "true"

[[invariant]]
name = "PaymentCaptured"
when = ["Active"]
assert = "payment_captured"
"#,
    );
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:Payment:weaker-snapshot-contract";
    let mut old_snapshot = crate::entity_actor::effects::build_initial_entity_state(
        "Payment",
        "weaker-snapshot-contract",
        &old_table,
        &serde_json::json!({"payment_captured": false}),
    )
    .expect("build old snapshot state");
    assert!(!old_snapshot.booleans["payment_captured"]);
    old_snapshot.sequence_nr = 5;
    old_snapshot.total_event_count = 5;
    let snapshot_bytes =
        EntityActor::serialize_snapshot_state(&old_snapshot, &old_table).expect("old snapshot");
    store
        .save_snapshot(pid, 5, &snapshot_bytes)
        .await
        .expect("save old contracted snapshot");

    let recovered = recover_entity_state_from_store(
        "default",
        "Payment",
        "weaker-snapshot-contract",
        &new_table,
        &crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("contract mismatch must replay authoritative history");

    assert_eq!(recovered.sequence_nr, 0);
    assert!(recovered.booleans["payment_captured"]);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn full_replay_resets_materialized_protected_state_before_effects() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Counter"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "count"
type = "counter"
initial = "1"

[[action]]
name = "Increment"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "increment", var = "count" }]

[[invariant]]
name = "PositiveCount"
when = ["Active"]
assert = "count > 0"
"#,
    );
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:Counter:retry-reset";
    store
        .append(
            pid,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Increment".to_string(),
                payload: serde_json::json!({
                    "action": "Increment",
                    "from_status": "Active",
                    "to_status": "Active",
                    "timestamp": "2024-01-01T00:00:00Z",
                    "params": {}
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: pid.to_string(),
                },
            }],
        )
        .await
        .expect("append increment");
    let mut materialized = crate::entity_actor::effects::build_initial_entity_state(
        "Counter",
        "retry-reset",
        &table,
        &serde_json::json!({}),
    )
    .expect("build materialized state");
    materialized.counters.insert("count".into(), 2);
    materialized.fields["count"] = serde_json::json!(2);

    EntityActor::replay_events(
        &table,
        &crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
        &mut materialized,
        ReplayOptions {
            tenant: "default",
            blob_store: None,
            strict_journal_read: true,
            initial_fields: &serde_json::json!({}),
        },
    )
    .await
    .expect("full replay");

    assert_eq!(materialized.counters["count"], 2);
    assert_eq!(materialized.fields["count"], 2);

    store.fail_next_reads(pid, 1);
    let error = EntityActor::replay_events(
        &table,
        &crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
        &mut materialized,
        ReplayOptions {
            tenant: "default",
            blob_store: None,
            strict_journal_read: true,
            initial_fields: &serde_json::json!({}),
        },
    )
    .await
    .expect_err("strict retry replay must fail closed on journal read failure");
    assert!(
        error
            .to_string()
            .contains("failed to read events for replay")
    );
    assert_eq!(materialized.counters["count"], 1);
}

/// An action-backed entity may use its first action to establish invariants
/// that intentionally do not hold in the spec's pristine initial state. That
/// transient state must remain unreadable and must never become durable.
#[cfg(feature = "sim")]
#[tokio::test]
async fn creation_requires_initializing_action_before_persistence() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let source = r#"
[automaton]
name = "ToolCall"
states = ["Pending"]
initial = "Pending"
allow_indefinite_states = ["Pending"]

[[state]]
name = "agent_id"
type = "string"
initial = ""

[[action]]
name = "Initialize"
kind = "input"
from = ["Pending"]
params = ["agent_id"]

[[invariant]]
name = "RequiresAgentId"
when = ["Pending"]
assert = "agent_id != ''"
"#;
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(source)));
    let table_snapshot = table.read().unwrap().clone();
    let initial_state = crate::entity_actor::effects::build_initial_entity_state(
        "ToolCall",
        "tc-invalid",
        &table_snapshot,
        &serde_json::json!({}),
    )
    .expect("build typed initial state");
    let diagnostic =
        crate::entity_actor::effects::runtime_invariant_failure(&initial_state, &table_snapshot)
            .expect("empty agent_id must identify the blocking invariant");
    assert!(diagnostic.contains("RequiresAgentId"));
    let store = Arc::new(SimEventStore::no_faults(213));
    let boxed_store = crate::storage::BoxedEventStore::from_arc(store.clone());
    let system = ActorSystem::new("sim-runtime-invariant-create");
    let actor = EntityActor::with_persistence(
        "ToolCall",
        "tc-invalid",
        table,
        serde_json::json!({}),
        boxed_store,
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, "tc-invalid");

    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("uninitialized actor must remain available for initialization");
    assert!(
        !response.success,
        "uninitialized state must not be readable"
    );
    assert!(
        store
            .read_events("default:ToolCall:tc-invalid", 0)
            .await
            .expect("read invalid entity journal")
            .is_empty(),
        "invalid initial state must not persist a bootstrap event"
    );

    let response: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Initialize".to_string(),
                params: serde_json::json!({"agent_id": "agent-1"}),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("initializing action must be accepted");
    assert!(
        response.success,
        "initializing action must establish safety"
    );
    assert_eq!(response.state.fields["agent_id"], "agent-1");
    let events = store
        .read_events("default:ToolCall:tc-invalid", 0)
        .await
        .expect("read initialized entity journal");
    assert_eq!(events.len(), 1, "only the valid initialization is durable");
    let event: crate::entity_actor::EntityEvent =
        serde_json::from_value(events[0].payload.clone()).expect("decode initialization event");
    assert_eq!(event.action, "Initialize");
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn direct_field_update_cannot_bypass_runtime_invariant() {
    let source = r#"
[automaton]
name = "Goal"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "goal"
type = "string"
initial = ""

[[invariant]]
name = "GoalRequired"
when = ["Active"]
assert = "goal != ''"
"#;
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(source)));
    let system = ActorSystem::new("sim-runtime-invariant-field-update");
    let actor = EntityActor::new(
        "Goal",
        "goal-update-1",
        table,
        serde_json::json!({"goal": "ship safely"}),
    );
    let actor_ref = system.spawn(actor, "goal-update-1");

    let rejected: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"goal": ""}),
                replace: false,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("field update reply");
    assert!(!rejected.success);
    assert_eq!(rejected.state.fields["goal"], "ship safely");
    assert!(
        rejected
            .error
            .as_deref()
            .is_some_and(|error| error.contains("GoalRequired"))
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn direct_counter_field_update_is_synchronized_and_rejected_atomically() {
    let source = r#"
[automaton]
name = "Workspace"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "used_bytes"
type = "counter"
initial = "0"

[[state]]
name = "quota_limit"
type = "counter"
initial = "10"

[[invariant]]
name = "WithinQuota"
when = ["Active"]
assert = "used_bytes <= quota_limit"
"#;
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(source)));
    let system = ActorSystem::new("sim-runtime-invariant-counter-field-update");
    let actor = EntityActor::new("Workspace", "ws-update-1", table, serde_json::json!({}));
    let actor_ref = system.spawn(actor, "ws-update-1");

    let rejected: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"used_bytes": 11}),
                replace: false,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("counter field update reply");
    assert!(!rejected.success);
    assert_eq!(rejected.state.counters.get("used_bytes"), Some(&0));
    assert_eq!(rejected.state.counters.get("quota_limit"), Some(&10));
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn model_protected_action_param_does_not_implicitly_mutate_state() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let source = r#"
[automaton]
name = "Payment"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "payment_captured"
type = "bool"
initial = "true"

[[action]]
name = "Update"
kind = "input"
from = ["Active"]
to = "Active"
params = ["payment_captured"]

[[invariant]]
name = "PaymentCaptured"
when = ["Active"]
assert = "payment_captured"
"#;
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(source)));
    let store = Arc::new(SimEventStore::no_faults(213));
    let boxed_store = crate::storage::BoxedEventStore::from_arc(store.clone());
    let system = ActorSystem::new("sim-model-protected-action");
    let actor = EntityActor::with_persistence(
        "Payment",
        "payment-1",
        table,
        serde_json::json!({"payment_captured": false}),
        boxed_store,
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, "payment-1");
    let before: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("initial state");
    assert!(before.state.booleans["payment_captured"]);
    assert_eq!(before.state.fields["payment_captured"], true);
    let initial_events = store
        .read_events("default:Payment:payment-1", 0)
        .await
        .expect("initial journal");

    let response: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Update".to_string(),
                params: serde_json::json!({"payment_captured": false}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("action reply");

    assert!(response.success);
    assert_eq!(response.state.status, before.state.status);
    assert_eq!(response.state.counters, before.state.counters);
    assert_eq!(response.state.booleans, before.state.booleans);
    assert_eq!(response.state.fields, before.state.fields);
    let after_events = store
        .read_events("default:Payment:payment-1", 0)
        .await
        .expect("journal after rejection");
    assert_eq!(after_events.len(), initial_events.len() + 1);

    let replay_table = TransitionTable::from_ioa_source(source);
    let recovered = recover_entity_state_from_store(
        "default",
        "Payment",
        "payment-1",
        &replay_table,
        &crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect("protected no-op parameter must replay");
    assert!(recovered.booleans["payment_captured"]);
    assert_eq!(recovered.fields["payment_captured"], true);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn model_protected_patch_rolls_back_bool_and_literal_counter_state() {
    let source = r#"
[automaton]
name = "Payment"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "items"
type = "counter"
initial = "1"

[[state]]
name = "payment_captured"
type = "bool"
initial = "true"

[[invariant]]
name = "ReadyToSettle"
when = ["Active"]
assert = "items > 0 && payment_captured"
"#;
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(source)));
    let system = ActorSystem::new("sim-model-protected-patch");
    let actor = EntityActor::new("Payment", "payment-2", table, serde_json::json!({}));
    let actor_ref = system.spawn(actor, "payment-2");
    let before: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("initial state");
    let before_state = serde_json::to_value(&before.state).expect("serialize initial state");

    for fields in [
        serde_json::json!({"payment_captured": false}),
        serde_json::json!({"items": 0}),
    ] {
        let rejected: EntityResponse = actor_ref
            .ask(
                EntityMsg::UpdateFields {
                    fields,
                    replace: false,
                },
                Duration::from_secs(5),
            )
            .await
            .expect("patch reply");
        assert!(!rejected.success);
        assert_eq!(
            serde_json::to_value(&rejected.state).expect("serialize rejected state"),
            before_state
        );
    }
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn delete_rolls_back_before_persisting_runtime_invariant_violation() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        DELETE_INVARIANT_IOA,
    )));
    let store = Arc::new(SimEventStore::no_faults(213));
    let system = ActorSystem::new("sim-runtime-invariant-delete");
    let actor = EntityActor::with_persistence(
        "Document",
        "doc-delete-1",
        table,
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, "doc-delete-1");

    let rejected: EntityResponse = actor_ref
        .ask(EntityMsg::Delete, Duration::from_secs(5))
        .await
        .expect("delete reply");
    assert!(!rejected.success);
    assert_eq!(rejected.state.status, "Active");
    assert!(
        rejected
            .error
            .as_deref()
            .is_some_and(|error| error.contains("DeletedRequiresReason"))
    );
    let events = store
        .read_events("default:Document:doc-delete-1", 0)
        .await
        .expect("read document journal");
    assert_eq!(events.len(), 1, "only bootstrap Created may be durable");
    assert_eq!(events[0].event_type, "Created");
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn replay_rejects_invalid_persisted_tombstone() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    let (_guard, _clock, _id_gen) = install_deterministic_context(213);
    let table = TransitionTable::from_ioa_source(DELETE_INVARIANT_IOA);
    let store = Arc::new(SimEventStore::no_faults(213));
    let pid = "default:Document:doc-tombstone-1";
    let tombstone = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Deleted".to_string(),
        payload: serde_json::json!({
            "action": "Deleted",
            "from_status": "Active",
            "to_status": "Deleted",
            "timestamp": "2024-01-01T00:00:00Z",
            "params": {}
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };
    store.append(pid, 0, &[tombstone]).await.unwrap();

    let error = recover_entity_state_from_store(
        "default",
        "Document",
        "doc-tombstone-1",
        &table,
        &crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        true,
    )
    .await
    .expect_err("invalid tombstone must fail hydration");
    assert!(
        error
            .to_string()
            .contains("persisted tombstone violates runtime safety contract"),
        "unexpected tombstone error: {error}"
    );
}

/// A committed cross-entity-guarded transition must survive replay.
///
/// Regression: `File.StreamUpdated` carries a `cross_entity_state` guard on the
/// owning Workspace. The guard's boolean is pre-resolved at dispatch time and
/// injected into the eval context, but replay rebuilds the context *without*
/// the related entity in scope — so re-evaluating the guard during replay sees
/// the cross-entity precondition as unsatisfied. Replay must NOT re-gate a
/// durably-stored event: it must honor the stored `to_status` and re-apply the
/// transition's effects, or a File that committed `Created -> Ready` would
/// silently rehydrate back to `Created` (losing `has_content` and the version
/// bump). This proves the stored history wins over a replay-time guard miss.
#[tokio::test]
async fn replay_honors_committed_cross_entity_guarded_transition() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    // A minimal File-shaped automaton whose advancing action is gated on a
    // cross-entity Workspace status that replay cannot reconstruct.
    let file_table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[state]]
name = "version_count"
type = "counter"
initial = "0"

[[state]]
name = "has_content"
type = "bool"
initial = "false"

[[action]]
name = "Create"
kind = "input"
from = ["Created"]
to = "Created"
params = ["workspace_id"]

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created", "Ready"]
to = "Ready"
params = ["size_bytes"]
guard = [
  { type = "cross_entity_state", entity_type = "Workspace", entity_id_source = "workspace_id", forbidden_status = ["Frozen", "Archived"] },
]
effect = [
  { type = "increment", var = "version_count" },
  { type = "set_bool", var = "has_content", value = "true" },
]
"#,
    )));

    let store = Arc::new(SimEventStore::no_faults(7));
    let pid = "default:File:fl-replay-1";

    let event = |action: &str, from: &str, to: &str| PersistenceEnvelope {
        sequence_nr: 0,
        event_type: action.to_string(),
        payload: serde_json::json!({
            "action": action,
            "from_status": from,
            "to_status": to,
            "timestamp": "2024-01-01T00:00:00Z",
            "params": {}
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };

    // The committed history: a File that was created, then advanced to Ready by
    // a guarded StreamUpdated. No Workspace entity is in scope at replay time.
    store
        .append(pid, 0, &[event("Created", "", "Created")])
        .await
        .unwrap();
    store
        .append(pid, 1, &[event("Create", "Created", "Created")])
        .await
        .unwrap();
    store
        .append(pid, 2, &[event("StreamUpdated", "Created", "Ready")])
        .await
        .unwrap();

    let system = ActorSystem::new("sim-replay-cross-entity");
    let actor = EntityActor::with_persistence(
        "File",
        "fl-replay-1",
        file_table,
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, "fl-replay-1");

    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();

    assert!(response.success);
    // The committed StreamUpdated transition survives replay despite the guard
    // being unsatisfiable without the Workspace in scope.
    assert_eq!(
        response.state.status, "Ready",
        "a committed cross-entity-guarded transition must not be dropped on replay"
    );
    // Its effects were re-applied: content flag set, version bumped.
    assert_eq!(response.state.booleans.get("has_content"), Some(&true));
    assert_eq!(response.state.counters.get("version_count"), Some(&1));
}
