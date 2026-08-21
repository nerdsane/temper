use std::collections::BTreeMap;
use std::sync::Arc;

use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError,
};
use temper_server::state::TrajectoryEntry;
use temper_server::storage::{
    BackendLabel, BoxedEventStore, QueryPlaneStore, QueryProjectionFieldsRow, StorageStack,
    TrajectorySink,
};
use temper_store_turso::TursoEventStore;

#[derive(Clone)]
struct RecordingEventStore;

struct RecordingQueryPlane;

#[async_trait::async_trait]
impl QueryPlaneStore for RecordingQueryPlane {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        assert_eq!(
            (tenant, entity_type, entity_id, status),
            ("default", "Ticket", "t-1", "Open")
        );
        assert_eq!(fields["title"], "hello");
        assert_eq!(state["fields"]["title"], "hello");
        assert_eq!(sequence_nr, 7);
        Ok(())
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        assert_eq!(
            (tenant, entity_type, entity_id),
            ("default", "Ticket", "t-1")
        );
        Ok(())
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        assert_eq!(
            (tenant, entity_type, where_clause),
            ("default", "Ticket", "title = ?")
        );
        assert_eq!(params, vec!["hello".to_string()]);
        Ok(Some(vec!["t-1".to_string()]))
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        assert_eq!((tenant, entity_type), ("default", "Ticket"));
        assert_eq!(entity_ids, &["t-1".to_string()]);
        assert_eq!(field_names, &["title"]);
        Ok(Some(vec![QueryProjectionFieldsRow {
            entity_id: "t-1".to_string(),
            status: "Open".to_string(),
            fields: BTreeMap::from([("title".to_string(), Some("hello".to_string()))]),
        }]))
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(Some(vec![("default".to_string(), 1)]))
    }
}

struct RecordingTrajectorySink;

#[async_trait::async_trait]
impl TrajectorySink for RecordingTrajectorySink {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        assert_eq!(entry.tenant, "default");
        assert_eq!(entry.entity_type, "Ticket");
        assert_eq!(entry.entity_id, "t-1");
        assert_eq!(entry.action, "Create");
        Ok(())
    }
}

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

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        assert_eq!(appends.len(), 1);
        assert_eq!(appends[0].persistence_id, "default:Ticket:t-1");
        assert_eq!(appends[0].expected_sequence, 0);
        Ok(vec![PersistenceAppendResult {
            persistence_id: appends[0].persistence_id.clone(),
            sequence_nr: appends[0].events.len() as u64,
        }])
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
            .append_batch(&[PersistenceAppend {
                persistence_id: "default:Ticket:t-1".to_string(),
                expected_sequence: 0,
                events: events.clone(),
            }])
            .await
            .expect("append batch through dyn adapter"),
        vec![PersistenceAppendResult {
            persistence_id: "default:Ticket:t-1".to_string(),
            sequence_nr: 2,
        }]
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
    assert_eq!(
        store
            .list_entity_ids_limited("default", Some("Ticket"), 1)
            .await
            .expect("bounded list through dyn adapter"),
        vec![("Ticket".to_string(), "t-1".to_string())]
    );
}

#[test]
fn storage_stack_labels_backend_and_exposes_boxed_events() {
    let events = BoxedEventStore::new(RecordingEventStore);
    let stack = StorageStack::new(
        BackendLabel::Postgres,
        events.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );

    assert_eq!(stack.backend, BackendLabel::Postgres);
    assert!(Arc::ptr_eq(&stack.events.inner(), &events.inner()));
    assert!(stack.platform.is_none());
    assert!(stack.policies.is_none());
    assert!(stack.query_plane.is_none());
    assert!(stack.trajectory.is_none());
    assert!(stack.metadata.is_none());
}

#[tokio::test]
async fn storage_stack_exposes_query_plane_and_trajectory_capabilities() {
    let stack = StorageStack::new(
        BackendLabel::Postgres,
        BoxedEventStore::new(RecordingEventStore),
        None,
        None,
        None,
        None,
        Some(Arc::new(RecordingQueryPlane)),
        None,
        Some(Arc::new(RecordingTrajectorySink)),
        None,
        None,
    );

    let query_plane = stack.query_plane.as_ref().expect("query plane");
    query_plane
        .upsert_projection(
            "default",
            "Ticket",
            "t-1",
            "Open",
            &serde_json::json!({"title": "hello"}),
            &serde_json::json!({"fields": {"title": "hello"}}),
            7,
        )
        .await
        .expect("upsert projection");
    assert_eq!(
        query_plane
            .query_field_index("default", "Ticket", "title = ?", vec!["hello".to_string()])
            .await
            .expect("query field index"),
        Some(vec!["t-1".to_string()])
    );
    assert_eq!(
        query_plane
            .load_projection_fields_many("default", "Ticket", &["t-1".to_string()], &["title"])
            .await
            .expect("load projection fields")
            .expect("projection fields")[0]
            .fields["title"],
        Some("hello".to_string())
    );
    query_plane
        .remove_projection("default", "Ticket", "t-1")
        .await
        .expect("remove projection");
    assert_eq!(
        query_plane
            .projected_entity_counts_by_tenant()
            .await
            .expect("counts"),
        Some(vec![("default".to_string(), 1)])
    );

    stack
        .trajectory
        .as_ref()
        .expect("trajectory sink")
        .persist_trajectory_entry(&trajectory_entry())
        .await
        .expect("trajectory persisted");
}

#[tokio::test]
async fn storage_stack_from_turso_exposes_concrete_capability_handles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("stack.db");
    let db_url = format!("file:{}", db_path.display());
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create turso store");

    let stack = StorageStack::from_turso(turso);

    assert_eq!(stack.backend, BackendLabel::Turso);
    assert!(stack.postgres_pool.is_none());
    assert!(stack.turso.is_some());
    assert!(stack.platform.is_some());
    assert!(stack.policies.is_some());
    assert!(stack.query_plane.is_some());
    assert!(stack.trajectory.is_some());
    assert!(stack.metadata.is_some());
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

fn trajectory_entry() -> TrajectoryEntry {
    TrajectoryEntry {
        timestamp: "2026-04-29T00:00:00Z".to_string(),
        tenant: "default".to_string(),
        entity_type: "Ticket".to_string(),
        entity_id: "t-1".to_string(),
        action: "Create".to_string(),
        success: true,
        from_status: None,
        to_status: Some("Open".to_string()),
        error: None,
        agent_id: None,
        session_id: None,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: None,
        spec_governed: Some(true),
        agent_type: None,
        request_body: None,
        intent: None,
        matched_policy_ids: None,
        capture_seq: None,
    }
}
