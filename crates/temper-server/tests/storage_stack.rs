use std::sync::Arc;

use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, PersistenceError,
};
use temper_server::storage::{BackendLabel, BoxedEventStore, StorageStack};

#[derive(Clone)]
struct RecordingEventStore;

impl EventStore for RecordingEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        assert_eq!(persistence_id, "default:Ticket:t-1");
        assert_eq!(expected_sequence, 0);
        Ok(events.len() as u64)
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        assert_eq!(persistence_id, "default:Ticket:t-1");
        assert_eq!(from_sequence, 0);
        Ok(vec![test_envelope(1)])
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        assert_eq!(persistence_id, "default:Ticket:t-1");
        assert_eq!(sequence_nr, 1);
        assert_eq!(snapshot, b"snapshot");
        Ok(())
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        assert_eq!(persistence_id, "default:Ticket:t-1");
        Ok(Some((1, b"snapshot".to_vec())))
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        assert_eq!(tenant, "default");
        Ok(vec![("Ticket".to_string(), "t-1".to_string())])
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        assert_eq!(tenant, "default");
        assert_eq!(entity_type, "Ticket");
        Ok(vec!["t-1".to_string()])
    }
}

#[tokio::test]
async fn boxed_event_store_delegates_through_object_safe_adapter() {
    let store = BoxedEventStore::new(RecordingEventStore);
    let events = vec![test_envelope(1), test_envelope(2)];

    assert_eq!(
        store
            .append("default:Ticket:t-1", 0, &events)
            .await
            .expect("append through dyn adapter"),
        2
    );
    assert_eq!(
        store
            .read_events("default:Ticket:t-1", 0)
            .await
            .expect("read through dyn adapter")
            .len(),
        1
    );
    store
        .save_snapshot("default:Ticket:t-1", 1, b"snapshot")
        .await
        .expect("snapshot through dyn adapter");
    assert_eq!(
        store
            .load_snapshot("default:Ticket:t-1")
            .await
            .expect("load snapshot through dyn adapter")
            .expect("snapshot row")
            .0,
        1
    );
    assert_eq!(
        store
            .list_entity_ids("default")
            .await
            .expect("list through dyn adapter"),
        vec![("Ticket".to_string(), "t-1".to_string())]
    );
    assert_eq!(
        store
            .list_entity_ids_by_type("default", "Ticket")
            .await
            .expect("list by type through dyn adapter"),
        vec!["t-1".to_string()]
    );
}

#[test]
fn storage_stack_labels_backend_and_exposes_boxed_events() {
    let events = BoxedEventStore::new(RecordingEventStore);
    let stack = StorageStack::new(BackendLabel::Postgres, events.clone(), None, None, None);

    assert_eq!(stack.backend, BackendLabel::Postgres);
    assert!(Arc::ptr_eq(&stack.events.inner(), &events.inner()));
    assert!(stack.platform.is_none());
    assert!(stack.query_plane.is_none());
    assert!(stack.compatibility_store.is_none());
}

fn test_envelope(sequence_nr: u64) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr,
        event_type: "Ticket.Created".to_string(),
        payload: serde_json::json!({"id": "t-1"}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::nil(),
            causation_id: uuid::Uuid::nil(),
            correlation_id: uuid::Uuid::nil(),
            timestamp: chrono::Utc::now(),
            actor_id: "test".to_string(),
        },
    }
}
