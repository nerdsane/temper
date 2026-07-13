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

/// ARN-189: PATCH field updates must survive actor eviction/restart.
#[cfg(feature = "sim")]
#[tokio::test]
async fn patched_fields_survive_actor_restart() {
    use std::sync::Arc;
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(7));
    let entity_id = "arn189-patch-1";

    let system = ActorSystem::new("sim-arn189-patch-a");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, entity_id);

    let patch: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "persisted-title", "Priority": 3}),
                replace: false,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("patch ask");
    assert!(patch.success, "patch should succeed: {:?}", patch.error);
    assert_eq!(
        patch.state.fields.get("Title").and_then(|v| v.as_str()),
        Some("persisted-title")
    );

    // Drop generation 1; rehydrate from journal only.
    drop(actor_ref);
    drop(system);

    let system2 = ActorSystem::new("sim-arn189-patch-b");
    let actor2 = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref2 = system2.spawn(actor2, entity_id);
    let after: EntityResponse = actor_ref2
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("get after restart");
    assert!(after.success);
    assert_eq!(
        after.state.fields.get("Title").and_then(|v| v.as_str()),
        Some("persisted-title"),
        "PATCH fields must survive restart (ARN-189)"
    );
    assert_eq!(
        after.state.fields.get("Priority").and_then(|v| v.as_i64()),
        Some(3)
    );
}

/// ARN-189: PUT replace must survive restart and drop omitted fields.
#[cfg(feature = "sim")]
#[tokio::test]
async fn replaced_fields_survive_actor_restart_with_replace_semantics() {
    use std::sync::Arc;
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(11));
    let entity_id = "arn189-put-1";

    let system = ActorSystem::new("sim-arn189-put-a");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({"Title": "old", "KeepMe": "x"}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, entity_id);

    let put: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "new-only"}),
                replace: true,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("put ask");
    assert!(put.success, "{:?}", put.error);
    assert_eq!(
        put.state.fields.get("Title").and_then(|v| v.as_str()),
        Some("new-only")
    );
    assert!(
        put.state.fields.get("KeepMe").is_none(),
        "PUT replace drops omitted fields"
    );

    drop(actor_ref);
    drop(system);

    let system2 = ActorSystem::new("sim-arn189-put-b");
    let actor2 = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref2 = system2.spawn(actor2, entity_id);
    let after: EntityResponse = actor_ref2
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("get after restart");
    assert_eq!(
        after.state.fields.get("Title").and_then(|v| v.as_str()),
        Some("new-only"),
        "PUT fields must survive restart (ARN-189)"
    );
    assert!(
        after.state.fields.get("KeepMe").is_none(),
        "PUT replace semantics must survive restart"
    );
}

#[test]
fn apply_field_update_merge_and_replace() {
    let mut state = EntityState {
        entity_type: "Order".into(),
        entity_id: "o1".into(),
        status: "Draft".into(),
        item_count: 0,
        counters: Default::default(),
        booleans: Default::default(),
        lists: Default::default(),
        fields: serde_json::json!({"Title": "a", "Extra": 1}),
        events: Default::default(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: Default::default(),
    };
    crate::entity_actor::effects::apply_field_update(
        &mut state,
        &serde_json::json!({"Title": "b"}),
        false,
    );
    assert_eq!(
        state.fields.get("Title").and_then(|v| v.as_str()),
        Some("b")
    );
    assert_eq!(state.fields.get("Extra").and_then(|v| v.as_i64()), Some(1));

    crate::entity_actor::effects::apply_field_update(
        &mut state,
        &serde_json::json!({"Title": "c"}),
        true,
    );
    assert_eq!(
        state.fields.get("Title").and_then(|v| v.as_str()),
        Some("c")
    );
    assert!(state.fields.get("Extra").is_none());
    assert_eq!(state.fields.get("Id").and_then(|v| v.as_str()), Some("o1"));
    assert_eq!(
        state.fields.get("Status").and_then(|v| v.as_str()),
        Some("Draft")
    );
}

/// ARN-189: journal append failure must not acknowledge or retain the update.
#[cfg(feature = "sim")]
#[tokio::test]
async fn field_update_with_failed_journal_append_does_not_report_success() {
    use std::sync::Arc;
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(13));
    let entity_id = "arn189-fail-1";
    let pid = format!("default:Order:{entity_id}");

    let system = ActorSystem::new("sim-arn189-fail");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, entity_id);

    // Bootstrap Created append succeeds first.
    let started: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(started.success);

    // Next append fails once with concurrency violation.
    store.inject_concurrency_violations(&pid, 1);

    let response: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "never durable"}),
                replace: false,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        !response.success,
        "field update whose journal append failed must not claim success: {:?}",
        response.error
    );
    assert!(
        response
            .state
            .fields
            .get("Title")
            .and_then(|v| v.as_str())
            .is_none_or(|t| t != "never durable"),
        "failed update must not leave half-applied Title in reply state"
    );

    let retained: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        retained
            .state
            .fields
            .get("Title")
            .and_then(|v| v.as_str())
            .is_none_or(|t| t != "never durable"),
        "failed update must not persist in retained actor state"
    );
}
