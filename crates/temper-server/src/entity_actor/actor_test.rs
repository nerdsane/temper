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
async fn dst_update_fields_preserves_runtime_owned_field_authority() {
    let system = ActorSystem::new("dst");
    let actor = EntityActor::new(
        "Order",
        "order-owned-fields",
        order_table(),
        serde_json::json!({
            "Id": "forged-initial",
            "id": "forged-initial",
            "Status": "Delivered",
            "status": "Delivered",
            "has_spec": false,
            "ctx_owner_status": "Privileged",
            "Customer": "Alice"
        }),
    );
    let actor_ref = system.spawn(actor, "order-owned-fields");

    for replace in [false, true] {
        let response: EntityResponse = actor_ref
            .ask(
                EntityMsg::UpdateFields {
                    fields: serde_json::json!({
                        "Id": "forged-update",
                        "id": "forged-update",
                        "Status": "Delivered",
                        "status": "Delivered",
                        "has_spec": true,
                        "HasSpec": true,
                        "ctx_owner_status": "Privileged",
                        "Customer": "Bob"
                    }),
                    replace,
                    expected_precondition: None,
                },
                Duration::from_secs(1),
            )
            .await
            .expect("field update response");

        assert_eq!(response.state.fields["Id"], "order-owned-fields");
        assert_eq!(response.state.fields["id"], "order-owned-fields");
        assert_eq!(response.state.fields["Status"], "Draft");
        assert_eq!(response.state.fields["status"], "Draft");
        assert_eq!(response.state.fields["Customer"], "Bob");
        for reserved in ["has_spec", "HasSpec", "ctx_owner_status"] {
            assert!(
                response.state.fields.get(reserved).is_none(),
                "persisted {reserved} during replace={replace}"
            );
        }
    }
}

#[tokio::test]
async fn dst_update_fields_rejects_stale_authorization_without_a_journal() {
    let system = ActorSystem::new("dst");
    let actor = EntityActor::new(
        "Order",
        "order-cas",
        order_table(),
        serde_json::json!({"Owner": "alice"}),
    );
    let actor_ref = system.spawn(actor, "order-cas");

    let authorized: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("authorized snapshot");
    let expected =
        crate::entity_actor::effects::entity_authorization_precondition(&authorized.state);
    assert_eq!(authorized.state.sequence_nr, 0);

    let concurrent: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Owner": "mallory"}),
                replace: false,
                expected_precondition: None,
            },
            Duration::from_secs(1),
        )
        .await
        .expect("concurrent field update");
    assert!(concurrent.success);
    assert_eq!(concurrent.state.sequence_nr, 0);

    let stale: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Secret": "forged"}),
                replace: false,
                expected_precondition: Some(expected),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("stale field update response");
    assert!(!stale.success);
    assert_eq!(stale.state.fields["Owner"], "mallory");
    assert!(stale.state.fields.get("Secret").is_none());

    let fresh_precondition =
        crate::entity_actor::effects::entity_authorization_precondition(&stale.state);
    let fresh: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Label": "authorized"}),
                replace: false,
                expected_precondition: Some(fresh_precondition),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("fresh field update response");
    assert!(fresh.success);
    assert_eq!(fresh.state.fields["Label"], "authorized");

    let before_status_change =
        crate::entity_actor::effects::entity_authorization_precondition(&fresh.state);
    let cancelled: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "CancelOrder".to_string(),
                params: serde_json::json!({"Reason": "test"}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: None,
                expected_authorization_precondition: None,
            },
            Duration::from_secs(1),
        )
        .await
        .expect("status-changing action response");
    assert!(cancelled.success);
    assert_eq!(cancelled.state.status, "Cancelled");
    assert_eq!(cancelled.state.sequence_nr, 0);

    let stale_status: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Secret": "still-forged"}),
                replace: false,
                expected_precondition: Some(before_status_change),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("status-stale field update response");
    assert!(!stale_status.success);
    assert!(stale_status.state.fields.get("Secret").is_none());

    let deleted: EntityResponse = actor_ref
        .ask(
            EntityMsg::Delete {
                expected_authorization_precondition: None,
            },
            Duration::from_secs(1),
        )
        .await
        .expect("delete response");
    assert!(deleted.success);
    let after_delete: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Secret": "post-delete"}),
                replace: false,
                expected_precondition: None,
            },
            Duration::from_secs(1),
        )
        .await
        .expect("post-delete field update response");
    assert!(!after_delete.success);
    assert_eq!(after_delete.state.status, "Deleted");
    assert!(after_delete.state.fields.get("Secret").is_none());
}

#[tokio::test]
async fn dst_action_rejects_stale_authorization_but_allows_idempotent_reply() {
    let system = ActorSystem::new("dst");
    let actor = EntityActor::new(
        "Order",
        "order-action-cas",
        order_table(),
        serde_json::json!({"Owner": "alice"}),
    );
    let actor_ref = system.spawn(actor, "order-action-cas");
    let authorized: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("authorized action snapshot");
    let stale_precondition =
        crate::entity_actor::effects::entity_authorization_precondition(&authorized.state);

    let concurrent: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Owner": "mallory"}),
                replace: false,
                expected_precondition: None,
            },
            Duration::from_secs(1),
        )
        .await
        .expect("concurrent owner update");
    let stale_delete: EntityResponse = actor_ref
        .ask(
            EntityMsg::Delete {
                expected_authorization_precondition: Some(stale_precondition.clone()),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("stale delete response");
    assert!(!stale_delete.success);
    assert_eq!(stale_delete.state.status, "Draft");

    let stale: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "CancelOrder".to_string(),
                params: serde_json::json!({"Reason": "stale"}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: Some("cancel-request".to_string()),
                expected_authorization_precondition: Some(stale_precondition),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("stale action response");
    assert!(!stale.success);
    assert_eq!(stale.state.status, "Draft");

    let fresh_precondition =
        crate::entity_actor::effects::entity_authorization_precondition(&concurrent.state);
    let applied: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "CancelOrder".to_string(),
                params: serde_json::json!({"Reason": "fresh"}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: Some("cancel-request".to_string()),
                expected_authorization_precondition: Some(fresh_precondition.clone()),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("fresh action response");
    assert!(applied.success);
    assert_eq!(applied.state.status, "Cancelled");

    let retry: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "CancelOrder".to_string(),
                params: serde_json::json!({"Reason": "fresh"}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: Some("cancel-request".to_string()),
                expected_authorization_precondition: Some(fresh_precondition),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("idempotent action retry response");
    assert!(retry.success);
    assert_eq!(retry.state.status, "Cancelled");
    assert_eq!(
        retry.state.total_event_count,
        applied.state.total_event_count
    );
}

#[test]
fn snapshot_restore_canonicalizes_runtime_owned_fields() {
    let mut state = EntityActor::build_initial_state(
        "Order",
        "order-snapshot",
        &order_table().read().expect("table lock"),
        &serde_json::json!({}),
    );
    let mut snapshot = state.clone();
    snapshot.fields = serde_json::json!({
        "Id": "forged",
        "id": "forged",
        "Status": "Delivered",
        "status": "Delivered",
        "has_spec": false,
        "ctx_owner_status": "Privileged",
        "Customer": "Alice"
    });
    let bytes = serde_json::to_vec(&snapshot).expect("snapshot serialization");

    assert!(EntityActor::apply_snapshot_bytes(&mut state, 7, &bytes));
    assert_eq!(state.fields["Id"], "order-snapshot");
    assert_eq!(state.fields["id"], "order-snapshot");
    assert_eq!(state.fields["Status"], "Draft");
    assert_eq!(state.fields["status"], "Draft");
    assert_eq!(state.fields["Customer"], "Alice");
    for reserved in ["has_spec", "ctx_owner_status"] {
        assert!(state.fields.get(reserved).is_none(), "restored {reserved}");
    }
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
                expected_authorization_precondition: None,
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
                expected_authorization_precondition: None,
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
                expected_authorization_precondition: None,
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
                expected_authorization_precondition: None,
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
                expected_authorization_precondition: None,
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
                    expected_authorization_precondition: None,
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
                expected_authorization_precondition: None,
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
                    expected_authorization_precondition: None,
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
                expected_authorization_precondition: None,
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
                expected_authorization_precondition: None,
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
                expected_authorization_precondition: None,
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

/// ARN-189: PATCH-style field updates must be journaled — a merge applied via
/// `EntityMsg::UpdateFields` has to survive actor eviction/restart, or any
/// OData PATCH is silently lost the moment the actor rehydrates from the
/// event store.
#[cfg(feature = "sim")]
#[tokio::test]
async fn patched_fields_survive_actor_restart() {
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(7));
    let entity_id = "arn189-patch-1";
    let pid = format!("default:Order:{entity_id}");

    // Generation 1: live actor accepts the PATCH merge.
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
    let response: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "durable title", "Priority": 3}),
                replace: false,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(response.success);
    assert_eq!(
        response.state.fields.get("Title").and_then(|v| v.as_str()),
        Some("durable title"),
        "live merge must apply"
    );

    // Generation 2: fresh actor over the same store — replay is the only input.
    let system2 = ActorSystem::new("sim-arn189-patch-b");
    let actor2 = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref2 = system2.spawn(actor2, entity_id);
    let rehydrated: EntityResponse = actor_ref2
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        rehydrated
            .state
            .fields
            .get("Title")
            .and_then(|v| v.as_str()),
        Some("durable title"),
        "PATCHed field must survive restart via journal replay (pid {pid})"
    );
    assert_eq!(
        rehydrated
            .state
            .fields
            .get("Priority")
            .and_then(|v| v.as_i64()),
        Some(3),
        "all merged fields must survive restart"
    );
}

/// ARN-189: PUT-style replacement must also be journaled with REPLACE
/// semantics — after restart the replaced field set must match the live
/// result, including the absence of keys the replacement dropped.
#[cfg(feature = "sim")]
#[tokio::test]
async fn replaced_fields_survive_actor_restart_with_replace_semantics() {
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(11));
    let entity_id = "arn189-put-1";

    let system = ActorSystem::new("sim-arn189-put-a");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, entity_id);

    // First a merge that introduces a key the later replacement drops.
    let merged: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "before", "Legacy": "drop me"}),
                replace: false,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(merged.success);

    // PUT: full replacement (Id/Status are preserved by the live path).
    let replaced: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Title": "after"}),
                replace: true,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(replaced.success);
    assert!(
        replaced.state.fields.get("Legacy").is_none(),
        "live replacement must drop absent keys"
    );

    let system2 = ActorSystem::new("sim-arn189-put-b");
    let actor2 = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref2 = system2.spawn(actor2, entity_id);
    let rehydrated: EntityResponse = actor_ref2
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        rehydrated
            .state
            .fields
            .get("Title")
            .and_then(|v| v.as_str()),
        Some("after"),
        "replaced field value must survive restart"
    );
    assert!(
        rehydrated.state.fields.get("Legacy").is_none(),
        "replacement semantics must survive restart — dropped keys must not resurrect"
    );
    assert_eq!(
        rehydrated.state.fields.get("Id").and_then(|v| v.as_str()),
        Some(entity_id),
        "Id must be preserved through replace + replay"
    );
}

/// Delegating event store whose appends fail once `armed` is set (so a test
/// can start an actor normally and then fail exactly the append under test)
/// and whose snapshot saves fail once `fail_snapshots` is set (so a test can
/// model a stalled snapshot path while appends keep succeeding).
#[cfg(feature = "sim")]
struct AppendFuseStore {
    inner: temper_store_sim::SimEventStore,
    armed: std::sync::atomic::AtomicBool,
    fail_snapshots: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "sim")]
impl AppendFuseStore {
    fn no_faults(seed: u64) -> Self {
        Self {
            inner: temper_store_sim::SimEventStore::no_faults(seed),
            armed: std::sync::atomic::AtomicBool::new(false),
            fail_snapshots: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn fuse_err(&self) -> Option<temper_runtime::persistence::PersistenceError> {
        if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
            Some(temper_runtime::persistence::PersistenceError::Storage(
                "injected append failure (fuse armed)".to_string(),
            ))
        } else {
            None
        }
    }
}

#[cfg(feature = "sim")]
impl temper_runtime::persistence::EventStore for AppendFuseStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, temper_runtime::persistence::PersistenceError> {
        if let Some(e) = self.fuse_err() {
            return Err(e);
        }
        self.inner
            .append(persistence_id, expected_sequence, events)
            .await
    }

    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[temper_runtime::persistence::EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, temper_runtime::persistence::PersistenceError> {
        if let Some(e) = self.fuse_err() {
            return Err(e);
        }
        self.inner
            .append_with_index_rows(
                persistence_id,
                expected_sequence,
                events,
                key_rows,
                vector_rows,
                reconcile_vectors,
            )
            .await
    }

    async fn append_batch(
        &self,
        appends: &[temper_runtime::persistence::PersistenceAppend],
    ) -> Result<
        Vec<temper_runtime::persistence::PersistenceAppendResult>,
        temper_runtime::persistence::PersistenceError,
    > {
        if let Some(e) = self.fuse_err() {
            return Err(e);
        }
        self.inner.append_batch(appends).await
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, temper_runtime::persistence::PersistenceError> {
        self.inner.read_events(persistence_id, from_sequence).await
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), temper_runtime::persistence::PersistenceError> {
        if self
            .fail_snapshots
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(temper_runtime::persistence::PersistenceError::Storage(
                "injected snapshot failure (stalled snapshot path)".to_string(),
            ));
        }
        self.inner
            .save_snapshot(persistence_id, sequence_nr, snapshot)
            .await
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, temper_runtime::persistence::PersistenceError> {
        self.inner.load_snapshot(persistence_id).await
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, temper_runtime::persistence::PersistenceError> {
        self.inner.list_entity_ids(tenant).await
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, temper_runtime::persistence::PersistenceError> {
        self.inner
            .list_entity_ids_by_type(tenant, entity_type)
            .await
    }
}

/// ARN-189 fail-closed: when the journal append fails, a field update must
/// NOT report success — otherwise the caller believes a write is durable
/// while restart will lose it.
#[cfg(feature = "sim")]
#[tokio::test]
async fn field_update_with_failed_journal_append_does_not_report_success() {
    let store = Arc::new(AppendFuseStore::no_faults(13));
    let entity_id = "arn189-fail-1";

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

    // Let startup (bootstrap Created append) succeed, then arm the fuse so the
    // field-update append is the one that fails.
    let started: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(started.success);
    store.armed.store(true, std::sync::atomic::Ordering::SeqCst);

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
        "a field update whose journal append failed must not claim success"
    );
    assert!(
        response.state.fields.get("Title").is_none(),
        "failed update must not leave half-applied in-memory state"
    );

    // The actor's RETAINED state must also be unmutated — not just the
    // error reply's snapshot.
    let retained: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        retained.state.fields.get("Title").is_none(),
        "failed update must not persist in the actor's retained state"
    );
}

/// ARN-189: field updates consume the same event budget as spec actions.
/// With the snapshot path stalled (all snapshot failures are soft), sustained
/// PATCH traffic must be REJECTED at MAX_EVENTS_SINCE_SNAPSHOT instead of
/// growing a replay tail that makes the entity permanently unhydratable.
#[cfg(feature = "sim")]
#[tokio::test]
async fn field_updates_reject_when_event_budget_exhausted() {
    let store = Arc::new(AppendFuseStore::no_faults(17));
    // Snapshots fail from the start: models a stalled snapshot path while
    // appends keep succeeding.
    store
        .fail_snapshots
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let entity_id = "arn189-budget-1";

    let system = ActorSystem::new("sim-arn189-budget");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    );
    let actor_ref = system.spawn(actor, entity_id);

    let mut rejected_at = None;
    for i in 0..=MAX_EVENTS_SINCE_SNAPSHOT {
        let response: EntityResponse = actor_ref
            .ask(
                EntityMsg::UpdateFields {
                    fields: serde_json::json!({"counter": i}),
                    replace: false,
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        if !response.success {
            assert!(
                response
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("Event budget exhausted"),
                "rejection must be the event budget, got: {:?}",
                response.error
            );
            assert!(
                response.state.events_since_snapshot >= MAX_EVENTS_SINCE_SNAPSHOT,
                "rejection must fire only at the budget boundary"
            );
            rejected_at = Some(i);
            break;
        }
    }
    assert!(
        rejected_at.is_some(),
        "sustained field updates with a stalled snapshot path must hit the event budget, \
         not grow the replay tail unboundedly"
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
