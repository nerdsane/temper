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
fn state_materialization_rejects_a_baseline_over_the_serialized_byte_budget() {
    let mut state = EntityState {
        entity_type: "Document".to_string(),
        entity_id: "oversized-baseline".to_string(),
        status: "Active".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({
            "Id": "oversized-baseline",
            "Status": "Active",
        }),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    };
    state.fields["Body"] = serde_json::Value::String("x".repeat(16 * 1024 * 1024));

    let error = state_materialization_envelope(
        "default:Document:oversized-baseline",
        &state,
        chrono::DateTime::UNIX_EPOCH,
    )
    .expect_err("oversized snapshot baselines must fail before cloning the state");

    assert!(
        error
            .to_string()
            .contains("state materialization byte budget exhausted"),
        "unexpected error: {error}"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn recovery_fails_closed_when_replayed_overflow_blob_cannot_be_persisted() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Document"
states = ["Active"]
initial = "Active"

[[action]]
name = "ReplaceBody"
kind = "input"
from = ["Active"]
to = "Active"
params = ["Body"]
"#,
    );
    let store = SimEventStore::no_faults(295);
    let boxed_store = crate::storage::BoxedEventStore::new(store.clone());
    let persistence_id = "default:Document:overflow-recovery";
    let timestamp = chrono::DateTime::UNIX_EPOCH;
    store
        .append(
            persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "ReplaceBody".to_string(),
                payload: serde_json::to_value(EntityEvent {
                    action: "ReplaceBody".to_string(),
                    from_status: "Active".to_string(),
                    to_status: "Active".to_string(),
                    timestamp,
                    params: serde_json::json!({"Body": "x".repeat(200 * 1024)}),
                    idempotency_key: None,
                })
                .expect("encode replay event"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: persistence_id.to_string(),
                },
            }],
        )
        .await
        .expect("seed durable event");

    let result = recover_entity_state_from_store(
        EntityRecoveryContext {
            tenant: "default",
            entity_type: "Document",
            entity_id: "overflow-recovery",
            table: &table,
            store: &boxed_store,
            backend: BackendLabel::Turso,
            initial_fields: &serde_json::json!({}),
            blob_store: None,
        },
        true,
    )
    .await;

    let error = result.expect_err("recovery must not publish dangling blob references");
    assert!(
        error
            .to_string()
            .contains("field-overflow blob persistence failed during replay"),
        "unexpected recovery error: {error}"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn materialized_unsnapshotted_durable_tail_restarts_at_the_admitted_budget() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Meter"
states = ["Active"]
initial = "Active"

[[action]]
name = "Touch"
kind = "input"
from = ["Active"]
to = "Active"
"#,
    );
    let store = SimEventStore::no_faults(294);
    let boxed_store = crate::storage::BoxedEventStore::new(store.clone());
    let persistence_id = "default:Meter:materialized-budget";
    let timestamp = chrono::DateTime::UNIX_EPOCH;
    let baseline = EntityState {
        entity_type: "Meter".to_string(),
        entity_id: "materialized-budget".to_string(),
        status: "Active".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({"Id": "materialized-budget", "Status": "Active"}),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    };
    let mut envelopes = Vec::with_capacity(MAX_EVENTS_SINCE_SNAPSHOT);
    envelopes.push(
        state_materialization_envelope(persistence_id, &baseline, timestamp)
            .expect("encode state materialization"),
    );
    for _ in 1..MAX_EVENTS_SINCE_SNAPSHOT {
        envelopes.push(PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "Touch".to_string(),
            payload: serde_json::to_value(EntityEvent {
                action: "Touch".to_string(),
                from_status: "Active".to_string(),
                to_status: "Active".to_string(),
                timestamp,
                params: serde_json::json!({}),
                idempotency_key: None,
            })
            .expect("encode Touch event"),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp,
                actor_id: persistence_id.to_string(),
            },
        });
    }
    store
        .append(persistence_id, 0, &envelopes)
        .await
        .expect("seed the largest admitted unsnapshotted durable tail");

    let recovered = recover_entity_state_with_source_from_store(
        EntityRecoveryContext {
            tenant: "default",
            entity_type: "Meter",
            entity_id: "materialized-budget",
            table: &table,
            store: &boxed_store,
            backend: BackendLabel::Sim,
            initial_fields: &serde_json::json!({}),
            blob_store: None,
        },
        true,
    )
    .await
    .expect("the admitted raw journal tail must remain restartable");

    assert_eq!(recovered.state.sequence_nr, 10_000);
    assert_eq!(recovered.state.events_since_snapshot, 10_000);
    assert_eq!(recovered.state.total_event_count, 9_999);
    assert!(!recovered.state.can_accept_event());
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn snapshot_ahead_of_journal_is_not_claimed_as_a_lower_applied_snapshot() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let store = SimEventStore::no_faults(284);
    let boxed_store = crate::storage::BoxedEventStore::new(store.clone());
    let persistence_id = "default:Order:snapshot-ahead";
    let durable_snapshot = b"snapshot-5".to_vec();
    store
        .save_snapshot(persistence_id, 5, &durable_snapshot)
        .await
        .expect("seed migration snapshot");
    let mut state = EntityState {
        entity_type: "Order".to_string(),
        entity_id: "snapshot-ahead".to_string(),
        status: "Draft".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({"Id": "snapshot-ahead", "Status": "Draft"}),
        events: std::collections::VecDeque::new(),
        total_event_count: 2,
        events_since_snapshot: 2,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 2,
        processed_idempotency_keys: BTreeMap::new(),
    };
    let mut source = temper_runtime::persistence::SnapshotSourceFence::Exact {
        sequence_nr: 5,
        state: durable_snapshot.clone(),
    };

    let attempted = EntityActor::maybe_save_snapshot(
        &boxed_store,
        None,
        persistence_id,
        &mut state,
        &mut source,
        None,
    )
    .await
    .expect("skip lower journal-aligned snapshot");

    assert_eq!(attempted, None);
    assert_eq!(state.last_snapshot_sequence_nr, 0);
    assert_eq!(state.events_since_snapshot, 2);
    assert_eq!(
        store.load_snapshot(persistence_id).await.unwrap(),
        Some((5, durable_snapshot))
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn stable_recovery_preserves_journal_snapshot_nondeterministic_effect_values() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Parent"
states = ["Idle", "Active"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Active"
params = []
effect = [{ type = "spawn", entity_type = "Child", entity_id_source = "{uuid}", initial_action = "Begin", store_id_in = "last_child_id" }]
"#,
    );
    let store = SimEventStore::no_faults(405);
    let boxed_store = crate::storage::BoxedEventStore::new(store.clone());
    let persistence_id = "default:Parent:journal-snapshot";
    let event = EntityEvent {
        action: "Start".to_string(),
        from_status: "Idle".to_string(),
        to_status: "Active".to_string(),
        timestamp: sim_now(),
        params: serde_json::json!({}),
        idempotency_key: Some("start-once".to_string()),
    };
    store
        .append(
            persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: event.action.clone(),
                payload: serde_json::to_value(&event).expect("encode Start event"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: event.timestamp,
                    actor_id: persistence_id.to_string(),
                },
            }],
        )
        .await
        .expect("seed Start journal event");
    let snapshot_state = EntityState {
        entity_type: "Parent".to_string(),
        entity_id: "journal-snapshot".to_string(),
        status: "Active".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({
            "Id": "journal-snapshot",
            "Status": "Active",
            "last_child_id": "durable-child-id"
        }),
        events: std::collections::VecDeque::new(),
        total_event_count: 1,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 1,
        sequence_nr: 1,
        processed_idempotency_keys: BTreeMap::from([("start-once".to_string(), 1)]),
    };
    let snapshot = EntityActor::serialize_snapshot_state(&snapshot_state, Some(1))
        .expect("encode journal-aligned snapshot");
    store
        .save_snapshot(persistence_id, 1, &snapshot)
        .await
        .expect("seed journal-aligned snapshot");

    let recovered = recover_entity_state_from_stable_sources(EntityRecoveryContext {
        tenant: "default",
        entity_type: "Parent",
        entity_id: "journal-snapshot",
        table: &table,
        store: &boxed_store,
        backend: BackendLabel::Sim,
        initial_fields: &serde_json::json!({}),
        blob_store: None,
    })
    .await
    .expect("stable recovery should use the journal-aligned snapshot boundary");

    assert_eq!(
        recovered
            .state
            .expect("journal-backed state")
            .fields
            .get("last_child_id"),
        Some(&serde_json::json!("durable-child-id")),
        "replaying the snapshotted Start event must not generate a replacement child id"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn actor_recovery_rejects_snapshot_identity_mismatch() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let store = SimEventStore::no_faults(285);
    let boxed_store = crate::storage::BoxedEventStore::new(store.clone());
    let persistence_id = "default:Order:expected-id";
    let mismatched = serde_json::to_vec(&serde_json::json!({
        "entity_type": "Order",
        "entity_id": "different-id",
        "status": "Draft",
        "item_count": 0,
        "fields": {"Id": "different-id", "Status": "Draft"}
    }))
    .expect("serialize mismatched snapshot");
    store
        .save_snapshot(persistence_id, 5, &mismatched)
        .await
        .expect("seed mismatched snapshot");
    let table = order_table().read().expect("table lock").clone();
    let result = recover_entity_state_with_source_from_store(
        EntityRecoveryContext {
            tenant: "default",
            entity_type: "Order",
            entity_id: "expected-id",
            table: &table,
            store: &boxed_store,
            backend: BackendLabel::Sim,
            initial_fields: &serde_json::json!({}),
            blob_store: None,
        },
        true,
    )
    .await;
    let error = match result {
        Ok(_) => panic!("mismatched snapshot identity must fail closed"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("snapshot identity mismatch"),
        "unexpected recovery error: {error}"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn actor_recovery_rejects_snapshot_status_outside_the_transition_table() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let store = SimEventStore::no_faults(289);
    let boxed_store = crate::storage::BoxedEventStore::new(store.clone());
    let persistence_id = "default:Order:invalid-status";
    let snapshot = serde_json::to_vec(&serde_json::json!({
        "entity_type": "Order",
        "entity_id": "invalid-status",
        "status": "NotADeclaredState",
        "item_count": 0,
        "fields": {"Id": "invalid-status", "Status": "NotADeclaredState"}
    }))
    .expect("serialize invalid-status snapshot");
    store
        .save_snapshot(persistence_id, 5, &snapshot)
        .await
        .expect("seed invalid-status snapshot");
    let table = order_table().read().expect("table lock").clone();
    let result = recover_entity_state_with_source_from_store(
        EntityRecoveryContext {
            tenant: "default",
            entity_type: "Order",
            entity_id: "invalid-status",
            table: &table,
            store: &boxed_store,
            backend: BackendLabel::Sim,
            initial_fields: &serde_json::json!({}),
            blob_store: None,
        },
        true,
    )
    .await;
    let error = match result {
        Ok(_) => panic!("snapshot status outside the transition table must fail closed"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("snapshot has invalid status"),
        "unexpected recovery error: {error}"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn domain_action_named_like_materialization_replays_as_a_domain_event() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let store = SimEventStore::no_faults(290);
    let boxed_store = crate::storage::BoxedEventStore::new(store.clone());
    let persistence_id = "default:Collision:domain-action";
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Collision"
states = ["Draft", "Ready"]
initial = "Draft"

[[action]]
name = "Temper.Internal.StateMaterialization.v1"
kind = "input"
from = ["Draft"]
to = "Ready"
"#,
    );
    let timestamp = chrono::DateTime::UNIX_EPOCH;
    store
        .append(
            persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: STATE_MATERIALIZATION_EVENT_TYPE.to_string(),
                payload: serde_json::to_value(EntityEvent {
                    action: STATE_MATERIALIZATION_EVENT_TYPE.to_string(),
                    from_status: "Draft".to_string(),
                    to_status: "Ready".to_string(),
                    timestamp,
                    params: serde_json::json!({}),
                    idempotency_key: None,
                })
                .expect("serialize domain collision event"),
                metadata: EventMetadata {
                    event_id: uuid::Uuid::nil(),
                    causation_id: uuid::Uuid::nil(),
                    correlation_id: uuid::Uuid::nil(),
                    timestamp,
                    actor_id: persistence_id.to_string(),
                },
            }],
        )
        .await
        .expect("append legal domain action collision");

    let recovered = recover_entity_state_with_source_from_store(
        EntityRecoveryContext {
            tenant: "default",
            entity_type: "Collision",
            entity_id: "domain-action",
            table: &table,
            store: &boxed_store,
            backend: BackendLabel::Sim,
            initial_fields: &serde_json::json!({}),
            blob_store: None,
        },
        true,
    )
    .await
    .expect("domain collision must replay normally");
    assert_eq!(recovered.state.status, "Ready");
    assert_eq!(recovered.state.sequence_nr, 1);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn actor_recovery_rejects_out_of_position_state_materialization() {
    use temper_runtime::persistence::EventStore;
    use temper_store_sim::SimEventStore;

    let store = SimEventStore::no_faults(286);
    let boxed_store = crate::storage::BoxedEventStore::new(store.clone());
    let persistence_id = "default:Order:late-materialization";
    let metadata = || EventMetadata {
        event_id: uuid::Uuid::nil(),
        causation_id: uuid::Uuid::nil(),
        correlation_id: uuid::Uuid::nil(),
        timestamp: chrono::DateTime::UNIX_EPOCH,
        actor_id: persistence_id.to_string(),
    };
    let baseline = EntityState {
        entity_type: "Order".to_string(),
        entity_id: "late-materialization".to_string(),
        status: "Draft".to_string(),
        item_count: 9,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({"Id": "late-materialization", "Status": "Draft"}),
        events: std::collections::VecDeque::new(),
        total_event_count: 9,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    };
    store
        .append(
            persistence_id,
            0,
            &[
                PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: "Created".to_string(),
                    payload: serde_json::to_value(EntityEvent {
                        action: "Created".to_string(),
                        from_status: String::new(),
                        to_status: "Draft".to_string(),
                        timestamp: chrono::DateTime::UNIX_EPOCH,
                        params: serde_json::json!({}),
                        idempotency_key: None,
                    })
                    .unwrap(),
                    metadata: metadata(),
                },
                PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: STATE_MATERIALIZATION_EVENT_TYPE.to_string(),
                    payload: serde_json::to_value(PersistedStateMaterialization {
                        schema: STATE_MATERIALIZATION_SCHEMA.to_string(),
                        state: baseline,
                    })
                    .unwrap(),
                    metadata: metadata(),
                },
            ],
        )
        .await
        .expect("seed out-of-position materialization");
    let table = order_table().read().expect("table lock").clone();
    let result = recover_entity_state_with_source_from_store(
        EntityRecoveryContext {
            tenant: "default",
            entity_type: "Order",
            entity_id: "late-materialization",
            table: &table,
            store: &boxed_store,
            backend: BackendLabel::Sim,
            initial_fields: &serde_json::json!({}),
            blob_store: None,
        },
        true,
    )
    .await;
    let error = match result {
        Ok(_) => panic!("late state materialization must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("must be the first journal event"),
        "unexpected recovery error: {error}"
    );
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
    let mut snapshot_source = temper_runtime::persistence::SnapshotSourceFence::Absent;

    EntityActor::maybe_save_snapshot(
        &boxed_store,
        Some(&snapshot_queue),
        persistence_id,
        &mut state,
        &mut snapshot_source,
        None,
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
        &mut snapshot_source,
        None,
    )
    .await
    .expect("snapshot boundary observation should succeed");

    assert_eq!(state.last_snapshot_sequence_nr, 100);
    assert_eq!(state.events_since_snapshot, 1);
    assert!(matches!(
        snapshot_source,
        temper_runtime::persistence::SnapshotSourceFence::Exact {
            sequence_nr: 100,
            ..
        }
    ));
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn queued_snapshot_carries_the_applied_exact_source_into_the_next_write() {
    use temper_runtime::persistence::{EventStore, SnapshotSourceFence};
    use temper_store_sim::SimEventStore;

    let store = SimEventStore::no_faults(44);
    let boxed_store = crate::storage::BoxedEventStore::new(store.clone());
    let snapshot_queue = SnapshotWriteQueue::start(boxed_store.clone());
    let persistence_id = "default:Order:queued-snapshot-chain";
    let mut state = EntityState {
        entity_type: "Order".to_string(),
        entity_id: "queued-snapshot-chain".to_string(),
        status: "Draft".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({
            "Id": "queued-snapshot-chain",
            "Status": "Draft"
        }),
        events: std::collections::VecDeque::new(),
        total_event_count: 100,
        events_since_snapshot: 100,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 100,
        processed_idempotency_keys: BTreeMap::new(),
    };
    let mut source = SnapshotSourceFence::Absent;

    EntityActor::maybe_save_snapshot(
        &boxed_store,
        Some(&snapshot_queue),
        persistence_id,
        &mut state,
        &mut source,
        None,
    )
    .await
    .expect("enqueue first snapshot");
    for _ in 0..20 {
        if snapshot_queue.applied_sequence(persistence_id) == Some(100) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(snapshot_queue.applied_sequence(persistence_id), Some(100));

    state.sequence_nr = 200;
    state.total_event_count = 200;
    state.events_since_snapshot = 200;
    EntityActor::maybe_save_snapshot(
        &boxed_store,
        Some(&snapshot_queue),
        persistence_id,
        &mut state,
        &mut source,
        None,
    )
    .await
    .expect("enqueue second snapshot from the same actor source");
    for _ in 0..20 {
        if snapshot_queue.applied_sequence(persistence_id) == Some(200) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        snapshot_queue.applied_sequence(persistence_id),
        Some(200),
        "the second queued write must fence against the exact first snapshot"
    );
    assert_eq!(
        store
            .load_snapshot(persistence_id)
            .await
            .expect("load chained snapshot")
            .map(|(sequence_nr, _)| sequence_nr),
        Some(200)
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn passivation_snapshot_keeps_the_contract_that_produced_actor_state() {
    use temper_runtime::persistence::{
        EventStore, PersistenceError, decode_activated_key_contract,
    };
    use temper_store_sim::SimEventStore;

    const IOA: &str = r#"
[automaton]
name = "Doc"
states = ["New", "Ready"]
initial = "New"

[[state]]
name = "Path"
type = "string"
initial = ""

[[key]]
name = "path"
properties = ["Path"]

[[action]]
name = "Create"
kind = "input"
from = ["New"]
to = "Ready"
params = ["Path"]
"#;

    let store = SimEventStore::no_faults(297);
    let boxed = crate::storage::BoxedEventStore::new(store.clone());
    let mut epoch_one_table = TransitionTable::from_ioa_source(IOA);
    let signature = crate::key_index::declared_key_set_signature(&epoch_one_table.keys);
    let epoch_one = store
        .activate_key_index_contract("default", "Doc", &signature, false)
        .await
        .expect("activate epoch one");
    store
        .mark_key_index_backfilled("default", "Doc", &signature)
        .await
        .expect("publish epoch-one readiness");
    epoch_one_table.key_contract_activation_epoch = epoch_one;
    let shared_table = Arc::new(RwLock::new(epoch_one_table));
    let system = ActorSystem::new("passivation-contract-provenance");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "provenance",
            shared_table.clone(),
            serde_json::json!({}),
            boxed.clone(),
            crate::storage::BackendLabel::Sim,
        ),
        "provenance",
    );
    let created: EntityResponse = actor
        .ask(
            EntityMsg::Action {
                name: "Create".to_string(),
                params: serde_json::json!({"Path": "/owned"}),
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(1),
        )
        .await
        .expect("Create response");
    assert!(created.success, "Create failed: {:?}", created.error);

    let epoch_two = store
        .activate_key_index_contract("default", "Doc", &signature, false)
        .await
        .expect("activate epoch two");
    let mut epoch_two_table = TransitionTable::from_ioa_source(IOA);
    epoch_two_table.key_contract_activation_epoch = epoch_two;
    *shared_table.write().expect("table lock") = epoch_two_table;

    let passivation: super::super::types::EntityPassivationSnapshot = actor
        .ask(EntityMsg::GetPassivationSnapshot, Duration::from_secs(1))
        .await
        .expect("passivation response");
    assert_eq!(
        decode_activated_key_contract(&passivation.key_contract).1,
        Some(epoch_one),
        "a live table swap must not rewrite actor-state provenance"
    );
    let snapshot = serde_json::to_vec(&passivation.state).expect("encode test snapshot");
    let error = boxed
        .save_snapshot_if_source(
            "default:Doc:provenance",
            passivation.state.sequence_nr,
            &snapshot,
            &passivation.snapshot_source,
            Some(&passivation.key_contract),
        )
        .await
        .expect_err("epoch-one passivation snapshot must be stale under epoch two");
    assert!(matches!(
        error,
        PersistenceError::KeyContractActivationStale {
            activated_epoch,
            attempted_epoch: Some(attempted_epoch),
        } if activated_epoch == epoch_two && attempted_epoch == epoch_one
    ));
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

/// Verify that replay fails closed when a committed event cannot be decoded
/// against the current `EntityEvent` schema.
///
/// Skipping the malformed event would silently construct state from only a
/// prefix of the durable history. The actor must stop before serving that
/// partial state.
#[cfg(feature = "sim")]
#[tokio::test]
async fn replay_rejects_schema_mismatched_events_before_serving_state() {
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

    let error = actor_ref
        .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect_err("malformed durable history must stop the actor");

    assert_eq!(error, temper_runtime::actor::ActorError::Stopped);
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
