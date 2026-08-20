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

fn task_schema_pin(digest: &str) -> SchemaExecutionPin {
    SchemaExecutionPin {
        scope: temper_runtime::persistence::schema_deployment::SchemaScope {
            kind: temper_runtime::persistence::schema_deployment::SchemaScopeKind::Task,
            id: "task-114".to_string(),
        },
        bundle_digest: digest.to_string(),
    }
}

#[cfg(feature = "sim")]
async fn activate_schema_pin(store: &temper_store_sim::SimEventStore, pin: &SchemaExecutionPin) {
    use temper_runtime::persistence::schema_deployment::{
        ActivateSchemaBundle, ClaimSchemaVerification, ClaimSchemaVerificationOutcome,
        SchemaBundleRecord, SchemaDeploymentStore, SchemaOperationIdentity,
        SchemaVerificationReceipt, SubmitSchemaBundle, SubmitSchemaBundleOutcome,
    };

    let outcome = store
        .submit_schema_bundle(SubmitSchemaBundle {
            bundle: SchemaBundleRecord {
                tenant: "default".to_string(),
                scope: pin.scope.clone(),
                digest: pin.bundle_digest.clone(),
                predecessor_digest: None,
                canonical_csdl: "<Schema/>".to_string(),
                canonical_ioa: BTreeMap::new(),
                cedar_policies: BTreeMap::new(),
                wasm_module_digests: BTreeMap::new(),
                migration_module_name: None,
                migration_module_digest: None,
                migration_abi_version: None,
                canonical_budgets: "{}".to_string(),
            },
            idempotency_key: format!("submit:{}", pin.bundle_digest),
            request_digest: pin.bundle_digest.clone(),
            request_id: format!("request:{}", pin.bundle_digest),
        })
        .await
        .expect("schema fixture submission should succeed");
    let record = match outcome {
        SubmitSchemaBundleOutcome::Created(record)
        | SubmitSchemaBundleOutcome::Replayed(record) => record,
    };
    let claim = match store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: "default".to_string(),
            scope: pin.scope.clone(),
            bundle_digest: pin.bundle_digest.clone(),
            logical_now: 1,
            lease_expires_at: 10,
            operation: SchemaOperationIdentity {
                idempotency_key: format!("verify:{}", pin.bundle_digest),
                request_digest: pin.bundle_digest.clone(),
                request_id: format!("verify-request:{}", pin.bundle_digest),
            },
        })
        .await
        .expect("schema fixture verification claim should succeed")
    {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record,
    };
    let receipt_id = format!("verify:{}", pin.bundle_digest);
    let verified = store
        .finish_schema_verification(
            "default",
            &pin.scope,
            &pin.bundle_digest,
            claim.fence,
            SchemaVerificationReceipt {
                id: receipt_id.clone(),
                verifier_version: "test/v1".to_string(),
                input_digest: pin.bundle_digest.clone(),
                passed: true,
            },
        )
        .await
        .expect("schema fixture verification should succeed");
    store
        .activate_schema_bundle(ActivateSchemaBundle {
            tenant: "default".to_string(),
            scope: pin.scope.clone(),
            bundle_digest: pin.bundle_digest.clone(),
            expected_predecessor: None,
            expected_fence: verified.fence,
            verification_receipt_id: receipt_id,
            operation: SchemaOperationIdentity {
                idempotency_key: format!("activate:{}", pin.bundle_digest),
                request_digest: pin.bundle_digest.clone(),
                request_id: format!("activate-request:{}", pin.bundle_digest),
            },
        })
        .await
        .expect("schema fixture activation should succeed");
    assert_eq!(record.bundle.digest, pin.bundle_digest);
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

#[cfg(feature = "sim")]
#[tokio::test]
async fn scoped_actor_commits_immutable_schema_pin_to_state_and_events() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(114));
    let pin = task_schema_pin(&format!("sha256:{}", "a".repeat(64)));
    activate_schema_pin(store.as_ref(), &pin).await;
    let actor = EntityActor::with_persistence(
        "Order",
        "scoped-1",
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store.clone()),
        crate::storage::BackendLabel::Sim,
    )
    .with_schema_pin(pin.clone());
    let system = ActorSystem::new("scoped-pin");
    let actor_ref = system.spawn(actor, "scoped-pin-actor");

    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("scoped actor should start");
    assert_eq!(
        serde_json::from_value::<SchemaExecutionPin>(
            response.state.fields[SCHEMA_PIN_FIELD].clone()
        )
        .expect("state pin should decode"),
        pin
    );

    let rule = crate::trigger::ReactionRule {
        name: "scoped-reaction".to_string(),
        when: crate::trigger::ReactionTrigger {
            entity_type: "Order".to_string(),
            action: Some("AddItem".to_string()),
            to_state: Some("Draft".to_string()),
            guard: None,
        },
        then: crate::trigger::ReactionTarget {
            entity_type: "Payment".to_string(),
            action: "Create".to_string(),
            params: serde_json::json!({}),
            params_from: BTreeMap::new(),
        },
        resolve_target: crate::trigger::TargetResolver::SameId,
        principal: None,
        drop_ok: false,
    };
    let action: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "AddItem".to_string(),
                params: serde_json::json!({"ProductId": "scoped-product"}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: None,
                expected_sequence: None,
                reaction_context: Some(Box::new(crate::trigger::delivery::ReactionCommitContext {
                    rules: vec![rule],
                    authority: serde_json::json!({}),
                    depth: 0,
                    root_delivery_id: None,
                    expected_source_sequence: response.state.sequence_nr,
                    resolved_guards: BTreeMap::new(),
                    receipt: Some(crate::trigger::delivery::ReactionReceipt {
                        delivery_id: "incoming-scoped-delivery".to_string(),
                        fencing_token: 7,
                        received_at: sim_now(),
                        schema_pin: None,
                    }),
                })),
            },
            Duration::from_secs(1),
        )
        .await
        .expect("scoped action should execute");
    assert!(action.success);

    let persistence_id = format!("default:Order:scoped-1:schema:{}", pin.bundle_digest);
    let events = store
        .read_events(&persistence_id, 0)
        .await
        .expect("event read should succeed");
    let action_event = events
        .iter()
        .find(|event| event.event_type == "AddItem")
        .expect("scoped action event should be durable");
    let event_pin: SchemaEventPin =
        serde_json::from_value(action_event.payload[SCHEMA_PIN_FIELD].clone())
            .expect("event pin should decode");
    assert_eq!(event_pin.execution, pin);
    assert!(event_pin.action_digest.starts_with("sha256:"));
    let intents = crate::trigger::delivery::extract_intents(&action_event.payload)
        .expect("reaction intents should decode");
    assert_eq!(intents[0].schema_pin.as_ref(), Some(&event_pin));
    let receipt = crate::trigger::delivery::extract_receipt(&action_event.payload)
        .expect("reaction receipt should decode")
        .expect("reaction receipt should be present");
    assert_eq!(receipt.schema_pin.as_ref(), Some(&event_pin));
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn scoped_actor_recovery_rejects_mismatched_event_pin() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let store = Arc::new(SimEventStore::no_faults(115));
    let expected = task_schema_pin(&format!("sha256:{}", "b".repeat(64)));
    let wrong = task_schema_pin(&format!("sha256:{}", "c".repeat(64)));
    activate_schema_pin(store.as_ref(), &expected).await;
    let persistence_id = format!("default:Order:scoped-2:schema:{}", expected.bundle_digest);
    let envelope = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Created".to_string(),
        payload: serde_json::json!({
            "action": "Created",
            "from_status": "",
            "to_status": "Draft",
            "timestamp": "2024-01-01T00:00:00Z",
            "params": {},
            SCHEMA_PIN_FIELD: SchemaEventPin {
                execution: wrong,
                action_digest: format!("sha256:{}", "d".repeat(64)),
            }
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.clone(),
        },
    };
    store
        .append(&persistence_id, 0, &[envelope])
        .await
        .expect("fixture append should succeed");

    let actor = EntityActor::with_persistence(
        "Order",
        "scoped-2",
        order_table(),
        serde_json::json!({}),
        crate::storage::BoxedEventStore::from_arc(store),
        crate::storage::BackendLabel::Sim,
    )
    .with_schema_pin(expected);
    let system = ActorSystem::new("scoped-pin-mismatch");
    let actor_ref = system.spawn(actor, "scoped-pin-mismatch-actor");
    let result = actor_ref
        .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(1))
        .await;
    assert!(
        result.is_err(),
        "mismatched durable pin must fail actor startup"
    );
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
async fn sequence_preconditions_are_checked_atomically_by_actor() {
    let system = ActorSystem::new("sequence-precondition");
    let actor = EntityActor::new("Order", "order-seq", order_table(), serde_json::json!({}));
    let actor_ref = system.spawn(actor, "order-seq");

    let action: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "AddItem".into(),
                params: serde_json::json!({"ProductId": "prod-1"}),
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                expected_sequence: Some(99),
                reaction_context: None,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(!action.success);
    assert_eq!(action.error.as_deref(), Some("SequenceConflict"));
    assert!(action.state.events.is_empty());

    let patch: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Name": "must-not-apply"}),
                replace: false,
                reference_evidence: std::collections::BTreeMap::new(),
                expected_sequence: Some(99),
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(!patch.success);
    assert_eq!(patch.error.as_deref(), Some("SequenceConflict"));
    assert!(patch.state.fields.get("Name").is_none());

    let sequence = patch.state.sequence_nr;
    let applied: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Name": "applied"}),
                replace: false,
                reference_evidence: std::collections::BTreeMap::new(),
                expected_sequence: Some(sequence),
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(applied.success);
    assert_eq!(applied.state.sequence_nr, sequence + 1);
    assert_eq!(applied.state.total_event_count, 1);
    assert_eq!(applied.state.events_since_snapshot, 1);
    assert_eq!(
        applied
            .state
            .events
            .back()
            .map(|event| event.action.as_str()),
        Some(crate::entity_actor::types::FIELD_UPDATE_EVENT_TYPE)
    );
    let stale: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Name": "stale"}),
                replace: false,
                reference_evidence: std::collections::BTreeMap::new(),
                expected_sequence: Some(sequence),
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(!stale.success);
    assert_eq!(stale.error.as_deref(), Some("SequenceConflict"));
}

#[tokio::test]
async fn scoped_schema_pin_cannot_be_replaced_or_removed_by_field_updates() {
    let system = ActorSystem::new("schema-pin-field-guard");
    let pin =
        task_schema_pin("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let actor = EntityActor::new(
        "Order",
        "order-scoped",
        order_table(),
        serde_json::json!({"Name": "before"}),
    )
    .with_schema_pin(pin.clone());
    let actor_ref = system.spawn(actor, "order-scoped");

    let forged: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({(SCHEMA_PIN_FIELD): {"bundle_digest": "forged"}}),
                replace: false,
                reference_evidence: BTreeMap::new(),
                expected_sequence: None,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(!forged.success);
    assert_eq!(forged.error.as_deref(), Some("ReservedFieldMutation"));

    let replaced: EntityResponse = actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields: serde_json::json!({"Name": "after"}),
                replace: true,
                reference_evidence: BTreeMap::new(),
                expected_sequence: Some(forged.state.sequence_nr),
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(replaced.success);
    assert_eq!(replaced.state.fields["Name"], "after");
    assert_eq!(
        serde_json::from_value::<SchemaExecutionPin>(
            replaced.state.fields[SCHEMA_PIN_FIELD].clone()
        )
        .unwrap(),
        pin
    );
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
                expected_sequence: None,
                reaction_context: None,
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
                expected_sequence: None,
                reaction_context: None,
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
                expected_sequence: None,
                reaction_context: None,
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
                expected_sequence: None,
                reaction_context: None,
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
                expected_sequence: None,
                reaction_context: None,
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
                    expected_sequence: None,
                    reaction_context: None,
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
                expected_sequence: None,
                reaction_context: None,
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
                    expected_sequence: None,
                    reaction_context: None,
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
                expected_sequence: None,
                reaction_context: None,
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
                expected_sequence: None,
                reaction_context: None,
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
                expected_sequence: None,
                reaction_context: None,
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
