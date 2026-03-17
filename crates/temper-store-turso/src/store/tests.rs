//! Integration tests for the Turso event store.

use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, PersistenceError,
};

use super::TursoEventStore;

fn test_envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "store-test".to_string(),
        },
    }
}

fn sqlite_test_url(test_name: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "temper-store-turso-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    format!("file:{}", path.display())
}

async fn make_store(test_name: &str) -> TursoEventStore {
    TursoEventStore::new(&sqlite_test_url(test_name), None)
        .await
        .expect("create store")
}

#[tokio::test]
async fn append_and_read_events_roundtrip() {
    let store = make_store("append-read").await;
    let persistence_id = "tenant-a:Order:ord-1";

    let new_seq = store
        .append(
            persistence_id,
            0,
            &[
                test_envelope("OrderCreated", serde_json::json!({ "id": "ord-1" })),
                test_envelope("OrderApproved", serde_json::json!({ "approved": true })),
            ],
        )
        .await
        .unwrap();

    assert_eq!(new_seq, 2);

    let events = store.read_events(persistence_id, 0).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[1].sequence_nr, 2);
    assert_eq!(events[0].event_type, "OrderCreated");
    assert_eq!(events[1].event_type, "OrderApproved");
}

#[tokio::test]
async fn append_with_wrong_sequence_fails_with_concurrency_violation() {
    let store = make_store("concurrency").await;
    let persistence_id = "tenant-a:Order:ord-2";

    store
        .append(
            persistence_id,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-2" }),
            )],
        )
        .await
        .unwrap();

    let err = store
        .append(
            persistence_id,
            0,
            &[test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 2 }),
            )],
        )
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));
}

#[tokio::test]
async fn snapshot_save_and_load_roundtrip() {
    let store = make_store("snapshot").await;
    let persistence_id = "tenant-a:Order:ord-3";

    store
        .save_snapshot(persistence_id, 5, b"{\"status\":\"created\"}")
        .await
        .unwrap();

    let snapshot = store.load_snapshot(persistence_id).await.unwrap();
    assert_eq!(snapshot, Some((5, b"{\"status\":\"created\"}".to_vec())));

    store
        .save_snapshot(persistence_id, 8, b"{\"status\":\"shipped\"}")
        .await
        .unwrap();

    let updated = store.load_snapshot(persistence_id).await.unwrap();
    assert_eq!(updated, Some((8, b"{\"status\":\"shipped\"}".to_vec())));
}

#[tokio::test]
async fn list_entity_ids_returns_distinct_pairs() {
    let store = make_store("entity-list").await;

    let tenant_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
    let tenant_b = format!("tenant-b-{}", uuid::Uuid::new_v4());

    let order_1 = format!("{tenant_a}:Order:ord-1");
    let order_2 = format!("{tenant_a}:Order:ord-2");
    let task_1 = format!("{tenant_a}:Task:task-1");
    let other_tenant = format!("{tenant_b}:Order:ord-9");

    store
        .append(
            &order_1,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-1" }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &order_1,
            1,
            &[test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 2 }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &order_2,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-2" }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &task_1,
            0,
            &[test_envelope(
                "TaskCreated",
                serde_json::json!({ "id": "task-1" }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &other_tenant,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-9" }),
            )],
        )
        .await
        .unwrap();

    let mut entities = store.list_entity_ids(&tenant_a).await.unwrap();
    entities.sort();

    assert_eq!(
        entities,
        vec![
            ("Order".to_string(), "ord-1".to_string()),
            ("Order".to_string(), "ord-2".to_string()),
            ("Task".to_string(), "task-1".to_string()),
        ]
    );
}

#[tokio::test]
async fn list_entity_ids_excludes_entities_with_deleted_tombstones() {
    let store = make_store("entity-list-deleted").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    let deleted_order = format!("{tenant}:Order:ord-deleted");
    let active_order = format!("{tenant}:Order:ord-active");

    store
        .append(
            &deleted_order,
            0,
            &[test_envelope(
                "Created",
                serde_json::json!({ "id": "ord-deleted" }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &deleted_order,
            1,
            &[test_envelope(
                "Deleted",
                serde_json::json!({
                    "action": "Deleted",
                    "from_status": "Draft",
                    "to_status": "Deleted"
                }),
            )],
        )
        .await
        .unwrap();
    store
        .append(
            &active_order,
            0,
            &[test_envelope(
                "Created",
                serde_json::json!({ "id": "ord-active" }),
            )],
        )
        .await
        .unwrap();

    let mut entities = store.list_entity_ids(&tenant).await.unwrap();
    entities.sort();

    assert_eq!(
        entities,
        vec![("Order".to_string(), "ord-active".to_string())]
    );
}

#[tokio::test]
async fn migrate_is_idempotent() {
    let store = make_store("migrate-idempotent").await;

    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
}

/// Regression: append must be durable (readable from a fresh connection)
/// before the caller receives the new sequence number.
///
/// This is the persist-before-return ordering guarantee: the event log must
/// reflect the written event for any subsequent reader, even one that opens
/// a new connection to the same database file.
#[tokio::test]
async fn append_is_durable_before_return() {
    let url = sqlite_test_url("persist-before-return");
    let store1 = TursoEventStore::new(&url, None)
        .await
        .expect("create store1");

    let persistence_id = "tenant-x:Widget:w-1";
    let new_seq = store1
        .append(
            persistence_id,
            0,
            &[test_envelope("Created", serde_json::json!({"id": "w-1"}))],
        )
        .await
        .expect("append");

    assert_eq!(new_seq, 1, "should return sequence 1 after first append");

    // Open a new independent connection to the same DB — simulates a second
    // reader or a process restart. The event must already be visible.
    let store2 = TursoEventStore::new(&url, None)
        .await
        .expect("create store2");
    let events = store2
        .read_events(persistence_id, 0)
        .await
        .expect("read from second connection");

    assert_eq!(
        events.len(),
        1,
        "event must be durable and readable from a fresh connection immediately after append"
    );
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[0].event_type, "Created");
}
