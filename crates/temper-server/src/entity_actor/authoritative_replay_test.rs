use super::*;
use crate::entity_actor::*;
use crate::storage::{BackendLabel, BoxedEventStore};
use temper_runtime::actor::ActorError;
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

#[derive(Clone, Default)]
struct StaticEventStore {
    events: Vec<PersistenceEnvelope>,
    snapshot: Option<(u64, Vec<u8>)>,
    read_error: Option<String>,
}

impl EventStore for StaticEventStore {
    async fn append(
        &self,
        _persistence_id: &str,
        _expected_sequence: u64,
        _events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        Err(PersistenceError::Storage(
            "static replay store is read-only".to_string(),
        ))
    }

    async fn append_batch(
        &self,
        _appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        Err(PersistenceError::Storage(
            "static replay store is read-only".to_string(),
        ))
    }

    async fn read_events(
        &self,
        _persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        if let Some(error) = &self.read_error {
            return Err(PersistenceError::Storage(error.clone()));
        }
        Ok(self
            .events
            .iter()
            .filter(|event| event.sequence_nr > from_sequence)
            .cloned()
            .collect())
    }

    async fn save_snapshot(
        &self,
        _persistence_id: &str,
        _sequence_nr: u64,
        _snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        Ok(())
    }

    async fn load_snapshot(
        &self,
        _persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        Ok(self.snapshot.clone())
    }

    async fn list_entity_ids(
        &self,
        _tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        Ok(Vec::new())
    }

    async fn list_entity_ids_by_type(
        &self,
        _tenant: &str,
        _entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        Ok(Vec::new())
    }
}

fn order_table() -> TransitionTable {
    TransitionTable::from_ioa_source(ORDER_IOA)
}

fn envelope(
    sequence_nr: u64,
    action: &str,
    from_status: &str,
    to_status: &str,
) -> PersistenceEnvelope {
    let event = EntityEvent {
        action: action.to_string(),
        from_status: from_status.to_string(),
        to_status: to_status.to_string(),
        timestamp: sim_now(),
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    PersistenceEnvelope {
        sequence_nr,
        event_type: action.to_string(),
        payload: serde_json::to_value(event).expect("test event should serialize"),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "default:Order:security-replay".to_string(),
        },
    }
}

async fn authoritative_replay(store: StaticEventStore) -> Result<EntityState, ActorError> {
    let initial_fields = serde_json::json!({});
    recover_authoritative_entity_state_from_store(
        "default",
        "Order",
        "security-replay",
        &order_table(),
        &BoxedEventStore::new(store),
        BackendLabel::Turso,
        &initial_fields,
        None,
    )
    .await
}

#[tokio::test]
async fn authoritative_replay_rejects_sequence_gaps() {
    let error = authoritative_replay(StaticEventStore {
        events: vec![
            envelope(1, "AddItem", "Draft", "Draft"),
            envelope(3, "CancelOrder", "Draft", "Cancelled"),
        ],
        ..StaticEventStore::default()
    })
    .await
    .expect_err("security replay must reject a journal gap");

    assert!(error.to_string().contains("non-contiguous journal"));
}

#[tokio::test]
async fn authoritative_replay_rejects_malformed_events() {
    let mut malformed = envelope(1, "CancelOrder", "Draft", "Cancelled");
    malformed.payload = serde_json::json!({"action": 42});
    let error = authoritative_replay(StaticEventStore {
        events: vec![malformed],
        ..StaticEventStore::default()
    })
    .await
    .expect_err("security replay must reject malformed history");

    assert!(error.to_string().contains("invalid event"));
}

#[tokio::test]
async fn authoritative_replay_ignores_ahead_stale_snapshot() {
    let table = order_table();
    let initial_fields = serde_json::json!({});
    let mut snapshot_state =
        EntityActor::build_initial_state("Order", "security-replay", &table, &initial_fields);
    snapshot_state.status = "Delivered".to_string();
    snapshot_state.fields["Status"] = serde_json::json!("Delivered");
    snapshot_state.sequence_nr = 99;
    snapshot_state.total_event_count = 99;
    let snapshot = EntityActor::serialize_snapshot_state(&snapshot_state)
        .expect("test snapshot should serialize");
    let store = StaticEventStore {
        events: vec![envelope(1, "CancelOrder", "Draft", "Cancelled")],
        snapshot: Some((99, snapshot)),
        read_error: None,
    };
    let boxed = BoxedEventStore::new(store.clone());

    let snapshot_recovery = recover_entity_state_from_store(
        "default",
        "Order",
        "security-replay",
        &table,
        &boxed,
        BackendLabel::Turso,
        &initial_fields,
        None,
        false,
    )
    .await
    .expect("ordinary recovery accepts the snapshot");
    assert_eq!(snapshot_recovery.status, "Delivered");

    let authoritative = authoritative_replay(store)
        .await
        .expect("complete journal is valid");
    assert_eq!(authoritative.status, "Cancelled");
    assert_eq!(authoritative.sequence_nr, 1);
}

#[tokio::test]
async fn authoritative_replay_propagates_journal_read_failure() {
    let error = authoritative_replay(StaticEventStore {
        read_error: Some("injected identity journal failure".to_string()),
        ..StaticEventStore::default()
    })
    .await
    .expect_err("security replay must fail closed on journal read failure");

    assert!(
        error
            .to_string()
            .contains("injected identity journal failure")
    );
}

#[tokio::test]
async fn authoritative_replay_rejects_history_after_tombstone() {
    let error = authoritative_replay(StaticEventStore {
        events: vec![
            envelope(1, "Deleted", "Draft", "Deleted"),
            envelope(2, "CancelOrder", "Draft", "Cancelled"),
        ],
        ..StaticEventStore::default()
    })
    .await
    .expect_err("security replay must treat tombstones as terminal");

    assert!(error.to_string().contains("after terminal tombstone"));
}

#[tokio::test]
async fn authoritative_replay_rejects_contradictory_tombstone() {
    let error = authoritative_replay(StaticEventStore {
        events: vec![envelope(1, "Deleted", "Draft", "Active")],
        ..StaticEventStore::default()
    })
    .await
    .expect_err("security replay must reject a non-terminal tombstone payload");

    assert!(error.to_string().contains("tombstone transition"));
}

#[tokio::test]
async fn authoritative_replay_rejects_envelope_payload_action_mismatch() {
    let mut mismatched = envelope(1, "AddItem", "Draft", "Draft");
    mismatched.event_type = "CancelOrder".to_string();
    let error = authoritative_replay(StaticEventStore {
        events: vec![mismatched],
        ..StaticEventStore::default()
    })
    .await
    .expect_err("security replay must bind envelope type to payload action");

    assert!(error.to_string().contains("differs from payload action"));
}

#[tokio::test]
async fn authoritative_replay_rejects_impossible_transition_history() {
    for event in [
        envelope(1, "CancelOrder", "Submitted", "Cancelled"),
        envelope(1, "AddItem", "Draft", "Delivered"),
    ] {
        let error = authoritative_replay(StaticEventStore {
            events: vec![event],
            ..StaticEventStore::default()
        })
        .await
        .expect_err("security replay must validate transition semantics");
        assert!(error.to_string().contains("incompatible event"));
    }
}

#[tokio::test]
async fn authoritative_replay_rejects_misbound_actor_metadata() {
    let mut misbound = envelope(1, "AddItem", "Draft", "Draft");
    misbound.metadata.actor_id = "default:Order:another-entity".to_string();
    let error = authoritative_replay(StaticEventStore {
        events: vec![misbound],
        ..StaticEventStore::default()
    })
    .await
    .expect_err("security replay must bind every event to its actor stream");

    assert!(error.to_string().contains("bound to actor"));
}
