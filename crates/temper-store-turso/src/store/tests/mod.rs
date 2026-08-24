//! Integration tests for the Turso event store.

use libsql::params;
use temper_runtime::persistence::{
    EntityVectorRow, EventMetadata, EventStore, PersistenceAppend, PersistenceEnvelope,
    PersistenceError, QueryProjectionOrder, QueryProjectionOrderTarget,
};

use super::{PublishedArtifactUpsert, QueryProjectionUpsert, TursoEventStore};
use crate::TursoSpecVerificationUpdate;

mod evolution_tenant;
mod policy_approval;

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
async fn trajectory_stats_never_cross_tenants() {
    // Each of the three statistics queries reads the shared `trajectories`
    // table, and the failed-intent list returns whole rows — error strings and
    // entity ids. One unfiltered query hands a tenant another tenant's
    // operational detail, so all three are asserted here (ADR-0157).
    let store = make_store("trajectory-stats-tenant").await;

    let row =
        |tenant: &'static str, action: &'static str, success: bool, error: Option<&'static str>| {
            crate::TursoTrajectoryInsert {
                tenant,
                entity_type: "Order",
                entity_id: "order-1",
                action,
                success,
                from_status: None,
                to_status: None,
                error,
                agent_id: Some("agent-1"),
                session_id: Some("session-1"),
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some("Entity"),
                spec_governed: Some(true),
                created_at: "2026-01-01T00:00:00Z",
                request_body: None,
                intent: None,
                matched_policy_ids: None,
                capture_seq: None,
            }
        };

    store
        .persist_trajectory(row("mine", "MyAction", true, None))
        .await
        .expect("mine ok");
    store
        .persist_trajectory(row(
            "theirs",
            "TheirAction",
            false,
            Some("their-secret-error"),
        ))
        .await
        .expect("theirs err");

    let stats = store
        .query_trajectory_stats("mine", None, None, None, 10)
        .await
        .expect("stats");

    assert_eq!(stats.total, 1, "totals must count only the caller's tenant");
    assert!(
        stats.by_action.contains_key("MyAction"),
        "the caller's own action is missing: {:?}",
        stats.by_action.keys().collect::<Vec<_>>()
    );
    assert!(
        !stats.by_action.contains_key("TheirAction"),
        "another tenant's action names leaked into by_action"
    );
    assert!(
        stats.failed_intents.iter().all(|r| r.tenant == "mine"),
        "another tenant's failed rows leaked into failed_intents"
    );
    assert!(
        !stats
            .failed_intents
            .iter()
            .any(|r| r.error.as_deref() == Some("their-secret-error")),
        "another tenant's error string leaked"
    );
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
async fn vector_index_write_behind_candidates_and_partitioning() {
    // ADR-0155: Turso maintains entity_vector_index write-behind (event first, index
    // follows). A candidate scan returns the partition's vectors in entity_id order,
    // partitioned by model tag; a raw kNN read never sees another model's vectors.
    let store = make_store("vector-index").await;
    let row = |decl: &str, model: &str, v: Vec<f32>| EntityVectorRow {
        decl_name: decl.to_string(),
        model_tag: model.to_string(),
        vector: v,
    };

    store
        .append_with_index_rows(
            "t:Item:item-b",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
            &[],
            &[row("embed", "m1", vec![0.0, 1.0])],
            true,
        )
        .await
        .unwrap();
    store
        .append_with_index_rows(
            "t:Item:item-a",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
            &[],
            &[row("embed", "m1", vec![1.0, 0.0])],
            true,
        )
        .await
        .unwrap();
    // A different model tag — must not appear in an m1 scan.
    store
        .append_with_index_rows(
            "t:Item:item-c",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
            &[],
            &[row("embed", "m2", vec![1.0, 0.0])],
            true,
        )
        .await
        .unwrap();

    let candidates = store
        .vector_candidates("t", "Item", "embed", "m1", 1000)
        .await
        .unwrap();
    // Two m1 items, in entity_id order (a before b) with their vectors intact.
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].entity_id, "item-a");
    assert_eq!(candidates[0].vector, vec![1.0, 0.0]);
    assert_eq!(candidates[1].entity_id, "item-b");
    assert_eq!(candidates[1].vector, vec![0.0, 1.0]);

    // Upsert: re-writing item-a's vector replaces (no duplicate row).
    store
        .backfill_entity_vectors("t", "Item", "item-a", &[row("embed", "m1", vec![0.5, 0.5])])
        .await
        .unwrap();
    let candidates = store
        .vector_candidates("t", "Item", "embed", "m1", 1000)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].vector, vec![0.5, 0.5]);

    // Watermark roundtrip + resumable id listing.
    store
        .mark_vector_index_backfilled("t", "Item", "embed")
        .await
        .unwrap();
    assert_eq!(
        store.vector_index_backfilled_types("t").await.unwrap(),
        vec![("Item".to_string(), "embed".to_string())]
    );
    let mut ids = store
        .vectored_entity_ids_for_type("t", "Item")
        .await
        .unwrap();
    ids.sort();
    assert_eq!(ids, vec!["item-a", "item-b", "item-c"]);
}

#[tokio::test]
async fn vector_index_reconcile_purges_on_delete_and_empty_rows() {
    // ADR-0155: a delete/clear reconciles to an empty row set, purging the entity's
    // vector rows (the turso-side "remove" cleanup) so it is never ranked again.
    let store = make_store("vector-purge").await;
    let row = |v: Vec<f32>| EntityVectorRow {
        decl_name: "embed".to_string(),
        model_tag: "m1".to_string(),
        vector: v,
    };

    // Write-behind reconcile with a row, then a delete transition (empty rows).
    store
        .append_with_index_rows(
            "t:Item:item-a",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
            &[],
            std::slice::from_ref(&row(vec![1.0, 0.0])),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .vector_candidates("t", "Item", "embed", "m1", 10)
            .await
            .unwrap()
            .len(),
        1
    );
    // A Deleted transition emits no vector rows but still reconciles (purge).
    store
        .append_with_index_rows(
            "t:Item:item-a",
            1,
            &[test_envelope("Delete", serde_json::json!({}))],
            &[],
            &[],
            true,
        )
        .await
        .unwrap();
    assert!(
        store
            .vector_candidates("t", "Item", "embed", "m1", 10)
            .await
            .unwrap()
            .is_empty(),
        "the deleted entity's vector row must be purged"
    );

    // The explicit backfill purge (empty rows) is idempotent.
    store
        .backfill_entity_vectors("t", "Item", "item-a", &[])
        .await
        .unwrap();
    assert!(
        store
            .vector_candidates("t", "Item", "embed", "m1", 10)
            .await
            .unwrap()
            .is_empty()
    );
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
async fn append_batch_zero_sequence_detects_existing_stream_by_unique_key() {
    let store = make_store("append-batch-zero-seq-conflict").await;
    let persistence_id = "tenant-a:Order:ord-batch-conflict";

    store
        .append(
            persistence_id,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-batch-conflict" }),
            )],
        )
        .await
        .unwrap();

    let err = store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 2 }),
            )],
        }])
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));

    let events = store.read_events(persistence_id, 0).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "OrderCreated");
}

#[tokio::test]
async fn single_event_append_bypasses_process_write_gate() {
    let mut store = make_store("single-append-bypasses-gate").await;
    store.write_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let held_gate = store
        .write_gate
        .clone()
        .acquire_owned()
        .await
        .expect("hold gate");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.append(
            "tenant-a:Order:ord-bypass",
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-bypass" }),
            )],
        ),
    )
    .await;
    drop(held_gate);

    let new_seq = result
        .expect("single-event append should not wait for the process write gate")
        .expect("append should succeed");
    assert_eq!(new_seq, 1);
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
async fn list_entity_ids_by_type_uses_entity_catalog() {
    let store = make_store("entity-list-by-type-catalog").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projection(
            &tenant,
            "AgentRoute",
            "route-main",
            "Ready",
            &serde_json::json!({ "Name": "main" }),
            3,
        )
        .await
        .expect("upsert AgentRoute projection");
    store
        .upsert_query_projection(
            &tenant,
            "Session",
            "session-1",
            "Completed",
            &serde_json::json!({ "Name": "session" }),
            1,
        )
        .await
        .expect("upsert Session projection");

    let ids = store
        .list_entity_ids_by_type(&tenant, "AgentRoute")
        .await
        .expect("list AgentRoute IDs by type");

    assert_eq!(ids, vec!["route-main".to_string()]);
}

#[tokio::test]
async fn list_entity_ids_by_type_unions_catalog_field_index_and_events() {
    let store = make_store("entity-list-by-type-union").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projection(
            &tenant,
            "AgentRoute",
            "route-catalog",
            "Ready",
            &serde_json::json!({ "Name": "catalog" }),
            3,
        )
        .await
        .expect("upsert catalog projection");
    store
        .upsert_query_projection(
            &tenant,
            "AgentRoute",
            "route-deleted",
            "Ready",
            &serde_json::json!({ "Name": "deleted" }),
            3,
        )
        .await
        .expect("upsert deleted projection");

    let conn = store.connection().expect("connection");
    conn.execute(
        "INSERT INTO entity_field_index \
         (tenant, entity_type, entity_id, field_name, field_value, status) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            tenant.clone(),
            "AgentRoute",
            "route-index",
            "Name",
            "index",
            "Ready"
        ],
    )
    .await
    .expect("insert field-index-only row");

    store
        .append(
            &format!("{tenant}:AgentRoute:route-event"),
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .expect("append event-only route");
    store
        .append(
            &format!("{tenant}:AgentRoute:route-deleted"),
            0,
            &[test_envelope("Deleted", serde_json::json!({}))],
        )
        .await
        .expect("append deleted tombstone");

    let ids = store
        .list_entity_ids_by_type(&tenant, "AgentRoute")
        .await
        .expect("list AgentRoute IDs by type");

    assert_eq!(
        ids,
        vec![
            "route-catalog".to_string(),
            "route-event".to_string(),
            "route-index".to_string(),
        ]
    );
}

#[tokio::test]
async fn list_entity_ids_by_type_includes_events_and_excludes_deleted() {
    let store = make_store("entity-list-by-type-events").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    let deleted_order = format!("{tenant}:Order:ord-deleted");
    let active_order = format!("{tenant}:Order:ord-active");
    let task = format!("{tenant}:Task:task-1");

    store
        .append(
            &deleted_order,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store
        .append(
            &deleted_order,
            1,
            &[test_envelope("Deleted", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store
        .append(
            &active_order,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store
        .append(&task, 0, &[test_envelope("Created", serde_json::json!({}))])
        .await
        .unwrap();

    let ids = store
        .list_entity_ids_by_type(&tenant, "Order")
        .await
        .expect("list Order IDs by type from events");

    assert_eq!(ids, vec!["ord-active".to_string()]);
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
async fn policy_denial_patterns_roundtrip_and_merge() {
    let store = make_store("policy-denials").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_policy_denial_pattern(
            &tenant,
            Some("planner"),
            "read",
            "Issue",
            "ISSUE-1",
            "2026-03-23T10:00:00Z",
        )
        .await
        .unwrap();
    store
        .upsert_policy_denial_pattern(
            &tenant,
            Some("planner"),
            "read",
            "Issue",
            "ISSUE-2",
            "2026-03-23T11:00:00Z",
        )
        .await
        .unwrap();

    let rows = store.load_policy_denial_patterns(&tenant).await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.agent_type.as_deref(), Some("planner"));
    assert_eq!(row.action, "read");
    assert_eq!(row.resource_type, "Issue");
    assert_eq!(row.count, 2);
    assert_eq!(row.first_seen, "2026-03-23T10:00:00Z");
    assert_eq!(row.last_seen, "2026-03-23T11:00:00Z");

    let ids: Vec<String> = serde_json::from_str(&row.distinct_resource_ids_json).unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"ISSUE-1".to_string()));
    assert!(ids.contains(&"ISSUE-2".to_string()));
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

#[tokio::test]
async fn query_projection_roundtrip_updates_catalog_and_field_index() {
    let store = make_store("query-projection-roundtrip").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let entity_type = "Order";
    let entity_id = "ord-projection";

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Draft",
            &serde_json::json!({
                "Title": "Projection Test",
                "Owner": "alice",
                "Count": 3,
            }),
            7,
        )
        .await
        .expect("upsert query projection");

    let title_matches = store
        .query_field_index(
            &tenant,
            entity_type,
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), "Projection Test".to_string()],
        )
        .await
        .expect("query field index by title");
    assert_eq!(title_matches, vec![entity_id.to_string()]);

    let counts = store
        .projected_entity_counts_by_tenant()
        .await
        .expect("load projected entity counts");
    assert_eq!(counts, vec![(tenant.clone(), 1)]);

    store
        .remove_query_projection(&tenant, entity_type, entity_id)
        .await
        .expect("remove query projection");

    let remaining = store
        .query_field_index(
            &tenant,
            entity_type,
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), "Projection Test".to_string()],
        )
        .await
        .expect("query field index after delete");
    assert!(
        remaining.is_empty(),
        "field index rows should be removed with the query projection"
    );

    let counts = store
        .projected_entity_counts_by_tenant()
        .await
        .expect("load projected entity counts after delete");
    assert!(
        counts.is_empty(),
        "entity catalog should be empty after removing the projection"
    );
}

#[tokio::test]
async fn query_field_index_page_orders_and_limits_inside_turso() {
    let store = make_store("query-field-index-page").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let entity_type = "SessionEntry";

    for sequence in [1_u64, 10, 2] {
        let entity_id = format!("entry-{sequence}");
        let fields = serde_json::json!({
            "SessionId": "ss-bounded",
            "Sequence": sequence,
        });
        let state = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "status": "Active",
            "fields": fields,
            "sequence_nr": sequence,
            "events": [],
        });
        store
            .upsert_query_projection_with_state(
                &tenant,
                entity_type,
                &entity_id,
                "Active",
                state.get("fields").unwrap(),
                &state,
                sequence,
            )
            .await
            .unwrap();
    }

    let (ids, count) = store
        .query_field_index_page(
            &tenant,
            entity_type,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 \
             AND field_name = ?3 AND field_value = ?4)",
            vec!["SessionId".to_string(), "ss-bounded".to_string()],
            &[QueryProjectionOrder {
                target: QueryProjectionOrderTarget::Property("Sequence".into()),
                descending: true,
            }],
            0,
            1,
            true,
        )
        .await
        .unwrap();

    assert_eq!(ids, vec!["entry-10".to_string()]);
    assert_eq!(count, Some(3));

    let (ids, count) = store
        .query_field_index_page(
            &tenant,
            entity_type,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 \
             AND field_name = ?3 AND field_value = ?4)",
            vec!["SessionId".to_string(), "ss-bounded".to_string()],
            &[QueryProjectionOrder {
                target: QueryProjectionOrderTarget::Property("Sequence".into()),
                descending: true,
            }],
            0,
            1,
            false,
        )
        .await
        .unwrap();

    assert_eq!(ids, vec!["entry-10".to_string()]);
    assert_eq!(count, None);

    let missing_sequence_id = "entry-missing-sequence";
    let missing_sequence_fields = serde_json::json!({
        "SessionId": "ss-bounded",
    });
    let missing_sequence_state = serde_json::json!({
        "entity_type": entity_type,
        "entity_id": missing_sequence_id,
        "status": "Active",
        "fields": missing_sequence_fields,
        "sequence_nr": 99,
        "events": [],
    });
    store
        .upsert_query_projection_with_state(
            &tenant,
            entity_type,
            missing_sequence_id,
            "Active",
            missing_sequence_state.get("fields").unwrap(),
            &missing_sequence_state,
            99,
        )
        .await
        .unwrap();

    let (ids, count) = store
        .query_field_index_page(
            &tenant,
            entity_type,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 \
             AND field_name = ?3 AND field_value = ?4)",
            vec!["SessionId".to_string(), "ss-bounded".to_string()],
            &[QueryProjectionOrder {
                target: QueryProjectionOrderTarget::Property("Sequence".into()),
                descending: true,
            }],
            0,
            1,
            true,
        )
        .await
        .unwrap();

    assert_eq!(ids, vec![missing_sequence_id.to_string()]);
    assert_eq!(count, Some(4));

    let (ids, _) = store
        .query_field_index_page(
            &tenant,
            entity_type,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 \
             AND field_name = ?3 AND field_value = ?4)",
            vec!["SessionId".to_string(), "ss-bounded".to_string()],
            &[QueryProjectionOrder {
                target: QueryProjectionOrderTarget::EntityCommitSequence,
                descending: true,
            }],
            0,
            2,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        ids,
        vec![missing_sequence_id.to_string(), "entry-10".to_string()]
    );

    let (ids, _) = store
        .query_field_index_page(
            &tenant,
            entity_type,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 \
             AND field_name = ?3 AND field_value = ?4)",
            vec!["SessionId".to_string(), "ss-bounded".to_string()],
            &[QueryProjectionOrder {
                target: QueryProjectionOrderTarget::EntityCommitSequence,
                descending: false,
            }],
            0,
            2,
            false,
        )
        .await
        .unwrap();
    assert_eq!(ids, vec!["entry-1".to_string(), "entry-2".to_string()]);
}

#[tokio::test]
async fn query_projection_batch_updates_catalog_and_field_index() {
    let store = make_store("query-projection-batch").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projections(
            &tenant,
            &[
                QueryProjectionUpsert {
                    entity_type: "Order".to_string(),
                    entity_id: "ord-batch-a".to_string(),
                    status: "Draft".to_string(),
                    fields: serde_json::json!({
                        "Title": "Batch A",
                        "Owner": "alice",
                    }),
                    state: serde_json::json!({
                        "entity_type": "Order",
                        "entity_id": "ord-batch-a",
                        "status": "Draft",
                        "fields": {
                            "Title": "Batch A",
                            "Owner": "alice",
                        },
                        "sequence_nr": 2,
                    }),
                    indexed_fields: serde_json::json!({
                        "Title": "Batch A",
                        "Owner": "alice",
                    }),
                    sequence_nr: 2,
                    known_new: false,
                },
                QueryProjectionUpsert {
                    entity_type: "Order".to_string(),
                    entity_id: "ord-batch-b".to_string(),
                    status: "Ready".to_string(),
                    fields: serde_json::json!({
                        "Title": "Batch B",
                        "Owner": "bob",
                    }),
                    state: serde_json::json!({
                        "entity_type": "Order",
                        "entity_id": "ord-batch-b",
                        "status": "Ready",
                        "fields": {
                            "Title": "Batch B",
                            "Owner": "bob",
                        },
                        "sequence_nr": 3,
                    }),
                    indexed_fields: serde_json::json!({
                        "Title": "Batch B",
                        "Owner": "bob",
                    }),
                    sequence_nr: 3,
                    known_new: false,
                },
            ],
        )
        .await
        .expect("batch projection upsert");

    let owner_matches = store
        .query_field_index(
            &tenant,
            "Order",
            "field_name = ?3 AND field_value = ?4",
            vec!["Owner".to_string(), "alice".to_string()],
        )
        .await
        .expect("query field index by owner");
    assert_eq!(owner_matches, vec!["ord-batch-a".to_string()]);

    let rows = store
        .load_entity_catalog_rows(
            &tenant,
            "Order",
            &["ord-batch-a".to_string(), "ord-batch-b".to_string()],
        )
        .await
        .expect("load catalog rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].sequence_nr, 2);
    assert_eq!(rows[1].sequence_nr, 3);
}

#[tokio::test]
async fn query_projection_batch_can_store_fields_without_indexing_them() {
    let store = make_store("query-projection-batch-index-subset").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projections(
            &tenant,
            &[QueryProjectionUpsert {
                entity_type: "Blob".to_string(),
                entity_id: "blob-index-subset".to_string(),
                status: "Durable".to_string(),
                fields: serde_json::json!({
                    "Id": "blob-index-subset",
                    "RepositoryId": "repo-1",
                    "CanonicalBytes": "full-canonical-payload",
                }),
                state: serde_json::json!({
                    "entity_type": "Blob",
                    "entity_id": "blob-index-subset",
                    "status": "Durable",
                    "fields": {
                        "Id": "blob-index-subset",
                        "RepositoryId": "repo-1",
                        "CanonicalBytes": "full-canonical-payload",
                    },
                    "sequence_nr": 1,
                }),
                indexed_fields: serde_json::json!({
                    "Id": "blob-index-subset",
                    "RepositoryId": "repo-1",
                }),
                sequence_nr: 1,
                known_new: true,
            }],
        )
        .await
        .expect("batch projection upsert");

    let rows = store
        .load_entity_catalog_rows(&tenant, "Blob", &["blob-index-subset".to_string()])
        .await
        .expect("load catalog row");
    assert_eq!(
        rows[0].fields["CanonicalBytes"],
        serde_json::json!("full-canonical-payload")
    );

    let canonical_matches = store
        .query_field_index(
            &tenant,
            "Blob",
            "field_name = ?3 AND field_value = ?4",
            vec![
                "CanonicalBytes".to_string(),
                "full-canonical-payload".to_string(),
            ],
        )
        .await
        .expect("query canonical field");
    assert!(
        canonical_matches.is_empty(),
        "filtered fields should stay out of entity_field_index"
    );
}

#[tokio::test]
async fn unchanged_projection_updates_catalog_without_rebuilding_field_rows() {
    let store = make_store("query-projection-stable-hash").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let entity_type = "Order";
    let entity_id = "ord-stable-projection";

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Draft",
            &serde_json::json!({
                "Title": "Projection Test",
                "Owner": "alice",
            }),
            7,
        )
        .await
        .expect("initial projection upsert");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT rowid FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND field_name = 'Title'",
            params![tenant.clone(), entity_type, entity_id],
        )
        .await
        .expect("query initial title row");
    let initial_row = rows
        .next()
        .await
        .expect("read initial title row")
        .expect("title row should exist");
    let initial_rowid = initial_row.get::<i64>(0).expect("initial rowid");

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Draft",
            &serde_json::json!({
                "Title": "Projection Test",
                "Owner": "alice",
            }),
            8,
        )
        .await
        .expect("second projection upsert");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT rowid FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND field_name = 'Title'",
            params![tenant.clone(), entity_type, entity_id],
        )
        .await
        .expect("query updated title row");
    let updated_row = rows
        .next()
        .await
        .expect("read updated title row")
        .expect("title row should still exist");
    let updated_rowid = updated_row.get::<i64>(0).expect("updated rowid");

    let mut catalog_rows = conn
        .query(
            "SELECT sequence_nr FROM entity_catalog \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .expect("query catalog row");
    let catalog_row = catalog_rows
        .next()
        .await
        .expect("read catalog row")
        .expect("catalog row should exist");
    let sequence_nr = catalog_row.get::<i64>(0).expect("catalog sequence_nr");

    assert_eq!(
        updated_rowid, initial_rowid,
        "unchanged projections should keep existing field index rows"
    );
    assert_eq!(
        sequence_nr, 8,
        "entity catalog should still advance to the latest sequence number"
    );
}

#[tokio::test]
async fn stale_projection_upsert_does_not_overwrite_newer_catalog_row() {
    let store = make_store("query-projection-stale-sequence-skip").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let entity_type = "App";
    let entity_id = "app-stale-projection";

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Active",
            &serde_json::json!({
                "OwnerId": "owner-a",
                "Name": "registered",
                "LatestVersionHash": "newer",
            }),
            4,
        )
        .await
        .expect("fresh projection upsert");

    store
        .upsert_query_projection(
            &tenant,
            entity_type,
            entity_id,
            "Active",
            &serde_json::json!({
                "Name": "registered",
                "RepositoryId": "repo-a",
            }),
            2,
        )
        .await
        .expect("stale projection upsert is ignored");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT sequence_nr, fields FROM entity_catalog \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .expect("query catalog row");
    let row = rows
        .next()
        .await
        .expect("read catalog row")
        .expect("catalog row should exist");
    let sequence_nr = row.get::<i64>(0).expect("catalog sequence_nr");
    let fields_json = row.get::<String>(1).expect("catalog fields");
    let fields: serde_json::Value =
        serde_json::from_str(&fields_json).expect("catalog fields are json");

    assert_eq!(sequence_nr, 4);
    assert_eq!(fields["OwnerId"], "owner-a");
    assert_eq!(fields["LatestVersionHash"], "newer");
    assert!(fields.get("RepositoryId").is_none());
}

#[tokio::test]
async fn load_query_projection_fields_many_returns_requested_fields_by_entity() {
    let store = make_store("query-projection-fields-many").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projection(
            &tenant,
            "File",
            "file-a",
            "Ready",
            &serde_json::json!({
                "content_hash": "sha256:file-a",
                "mime_type": "application/json",
                "has_content": true,
                "size_bytes": 12,
            }),
            1,
        )
        .await
        .expect("upsert file-a projection");
    store
        .upsert_query_projection(
            &tenant,
            "File",
            "file-b",
            "Created",
            &serde_json::json!({
                "content_hash": "",
                "mime_type": "text/plain",
                "has_content": false,
            }),
            1,
        )
        .await
        .expect("upsert file-b projection");

    let rows = store
        .load_query_projection_fields_many(
            &tenant,
            "File",
            &[
                "file-a".to_string(),
                "file-b".to_string(),
                "missing".to_string(),
            ],
            &["content_hash", "mime_type", "has_content"],
        )
        .await
        .expect("load projected fields");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].entity_id, "file-a");
    assert_eq!(rows[0].status, "Ready");
    assert_eq!(
        rows[0]
            .fields
            .get("content_hash")
            .and_then(|v| v.as_deref()),
        Some("sha256:file-a")
    );
    assert_eq!(
        rows[0].fields.get("mime_type").and_then(|v| v.as_deref()),
        Some("application/json")
    );
    assert_eq!(
        rows[0].fields.get("has_content").and_then(|v| v.as_deref()),
        Some("true")
    );

    assert_eq!(rows[1].entity_id, "file-b");
    assert_eq!(rows[1].status, "Created");
    assert_eq!(
        rows[1].fields.get("has_content").and_then(|v| v.as_deref()),
        Some("false")
    );
    assert!(
        rows.iter().all(|row| row.entity_id != "missing"),
        "missing entity ids should be omitted"
    );
}

#[tokio::test]
async fn load_entity_catalog_rows_returns_full_projected_fields() {
    let store = make_store("entity-catalog-rows-full-fields").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    let fields = serde_json::json!({
        "Path": "/notes/readme.md",
        "WorkspaceId": "ws-a",
        "MimeType": "text/markdown",
        "HasContent": true,
        "content_hash": "sha256:file-a",
        "has_content": true,
        "size_bytes": 12,
        "nested": { "kept": true },
    });
    let state = serde_json::json!({
        "entity_type": "File",
        "entity_id": "file-a",
        "status": "Ready",
        "item_count": 2,
        "counters": {"Views": 3},
        "booleans": {"Pinned": true},
        "lists": {"Tags": ["docs"]},
        "fields": fields.clone(),
        "events": [],
        "total_event_count": 7,
        "sequence_nr": 7,
    });
    store
        .upsert_query_projection_with_state(&tenant, "File", "file-a", "Ready", &fields, &state, 7)
        .await
        .expect("upsert file projection");

    let rows = store
        .load_entity_catalog_rows(
            &tenant,
            "File",
            &["file-a".to_string(), "missing".to_string()],
        )
        .await
        .expect("load catalog rows");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entity_id, "file-a");
    assert_eq!(rows[0].status, "Ready");
    assert_eq!(rows[0].sequence_nr, 7);
    assert_eq!(rows[0].fields["Path"], "/notes/readme.md");
    assert_eq!(rows[0].fields["WorkspaceId"], "ws-a");
    assert_eq!(rows[0].fields["MimeType"], "text/markdown");
    assert_eq!(rows[0].fields["HasContent"], true);
    assert_eq!(rows[0].fields["content_hash"], "sha256:file-a");
    assert_eq!(rows[0].fields["has_content"], true);
    assert_eq!(rows[0].fields["size_bytes"], 12);
    assert_eq!(rows[0].fields["nested"]["kept"], true);
    assert_eq!(rows[0].state.as_ref().unwrap()["counters"]["Views"], 3);
    assert_eq!(rows[0].state.as_ref().unwrap()["booleans"]["Pinned"], true);
    assert_eq!(rows[0].state.as_ref().unwrap()["lists"]["Tags"][0], "docs");
}

#[tokio::test]
async fn query_projection_status_follows_projected_state_over_fallback_argument() {
    let store = make_store("query-projection-status-state-parity").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let fields = serde_json::json!({
        "Title": "Default lifecycle row",
        "Status": "Draft",
    });
    let state = serde_json::json!({
        "entity_type": "Order",
        "entity_id": "ord-draft",
        "status": "Draft",
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": fields.clone(),
        "events": [],
        "total_event_count": 1,
        "sequence_nr": 1,
    });

    store
        .upsert_query_projection_with_state(
            &tenant,
            "Order",
            "ord-draft",
            "Published",
            &fields,
            &state,
            1,
        )
        .await
        .expect("upsert projection with stale fallback status");

    let rows = store
        .load_entity_catalog_rows(&tenant, "Order", &["ord-draft".to_string()])
        .await
        .expect("load catalog row");
    assert_eq!(rows[0].status, "Draft");

    let ids = store
        .query_field_index(&tenant, "Order", "status = ?3", vec!["Draft".to_string()])
        .await
        .expect("query catalog status");
    assert_eq!(ids, vec!["ord-draft".to_string()]);
}

#[tokio::test]
async fn published_artifact_upsert_round_trips_and_updates_by_id() {
    let store = make_store("published-artifacts").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    let artifact = PublishedArtifactUpsert {
        id: "part-test".to_string(),
        tenant: tenant.clone(),
        source_file_id: "fl-source".to_string(),
        source_file_version_id: "fv-source-v1".to_string(),
        content_hash: "sha256:first".to_string(),
        label: "preview".to_string(),
        mime_type: "image/png".to_string(),
        byte_length: 42,
        public_storage_key: "demo/documents/doc-1/preview-sha256:first.png".to_string(),
        public_url: "https://artifacts.example.com/demo/documents/doc-1/preview-sha256:first.png"
            .to_string(),
        owner_ref_type: "Document".to_string(),
        owner_ref_id: "doc-1".to_string(),
        status: "published".to_string(),
    };

    let inserted = store
        .upsert_published_artifact(&artifact)
        .await
        .expect("insert published artifact");
    assert_eq!(inserted.id, artifact.id);
    assert_eq!(inserted.public_url, artifact.public_url);

    let mut updated = artifact;
    updated.source_file_version_id = "fv-source-v2".to_string();
    updated.content_hash = "sha256:second".to_string();
    updated.byte_length = 84;
    updated.public_storage_key = "demo/documents/doc-1/preview-sha256:second.png".to_string();
    updated.public_url =
        "https://artifacts.example.com/demo/documents/doc-1/preview-sha256:second.png".to_string();

    store
        .upsert_published_artifact(&updated)
        .await
        .expect("update published artifact");
    let loaded = store
        .load_published_artifact(&tenant, "part-test")
        .await
        .expect("load published artifact")
        .expect("published artifact exists");

    assert_eq!(loaded.source_file_version_id, "fv-source-v2");
    assert_eq!(loaded.content_hash, "sha256:second");
    assert_eq!(loaded.byte_length, 84);
    assert_eq!(loaded.public_url, updated.public_url);
}

#[tokio::test]
async fn export_query_projections_returns_all_fields_for_migration() {
    let store = make_store("query-projection-export").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projection(
            &tenant,
            "File",
            "file-a",
            "Ready",
            &serde_json::json!({
                "content_hash": "sha256:file-a",
                "has_content": true,
                "size_bytes": 12,
            }),
            9,
        )
        .await
        .expect("upsert projection");

    let rows = store
        .export_query_projections(Some(&tenant))
        .await
        .expect("export query projections");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tenant, tenant);
    assert_eq!(rows[0].entity_type, "File");
    assert_eq!(rows[0].entity_id, "file-a");
    assert_eq!(rows[0].status, "Ready");
    assert_eq!(rows[0].sequence_nr, 9);
    assert_eq!(
        rows[0].fields.get("content_hash").and_then(|v| v.as_str()),
        Some("sha256:file-a")
    );
    assert_eq!(
        rows[0].fields.get("has_content").and_then(|v| v.as_str()),
        None
    );
    assert_eq!(rows[0].fields["has_content"], true);
    assert_eq!(
        rows[0].fields.get("size_bytes").and_then(|v| v.as_str()),
        None
    );
    assert_eq!(rows[0].fields["size_bytes"], 12);
}

#[tokio::test]
async fn list_blobs_returns_rows_for_migration() {
    let store = make_store("blob-list").await;

    store
        .put_blob("temper-fs/sha256:abc", b"hello")
        .await
        .expect("put blob");

    let rows = store.list_blobs(100).await.expect("list blobs");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].blob_key, "temper-fs/sha256:abc");
    assert_eq!(rows[0].data, b"hello");
    assert_eq!(rows[0].size_bytes, 5);
    assert!(!rows[0].created_at.is_empty());
    assert_eq!(rows[0].expires_at, None);
}

#[tokio::test]
async fn load_wasm_module_metadata_all_tenants_returns_metadata_without_bulk_bytes() {
    let store = make_store("wasm-metadata").await;

    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a", "bundled")
        .await
        .expect("persist mod-a");
    store
        .upsert_wasm_module("tenant-b", "mod-b", b"hello-b", "hash-b", "bundled")
        .await
        .expect("persist mod-b");

    let rows = store
        .load_wasm_module_metadata_all_tenants()
        .await
        .expect("load wasm metadata");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].tenant, "tenant-a");
    assert_eq!(rows[0].module_name, "mod-a");
    assert_eq!(rows[0].sha256_hash, "hash-a");
    assert_eq!(rows[0].size_bytes, 7);
    assert!(!rows[0].updated_at.is_empty());
    assert_eq!(rows[1].tenant, "tenant-b");
    assert_eq!(rows[1].module_name, "mod-b");
    assert_eq!(rows[1].sha256_hash, "hash-b");
    assert_eq!(rows[1].size_bytes, 7);
    assert!(!rows[1].updated_at.is_empty());

    let metadata_row = store
        .load_wasm_module("tenant-a", "mod-a")
        .await
        .expect("load wasm metadata row")
        .expect("metadata row should exist");
    assert!(
        metadata_row.wasm_bytes.is_empty(),
        "Turso rows stay metadata-only; artifact bytes live in object storage"
    );
}

#[tokio::test]
async fn upsert_specs_and_commit_preserves_identical_spec_version() {
    let store = make_store("spec-idempotent").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("initial spec commit");
    store
        .persist_spec_verification(
            &tenant,
            "Issue",
            TursoSpecVerificationUpdate {
                status: "passed",
                verified: true,
                levels_passed: Some(1),
                levels_total: Some(1),
                verification_result_json: Some(r#"{"all_passed":true}"#),
            },
        )
        .await
        .expect("persist verification");

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("identical spec commit");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT version, verified, verification_status, committed \
             FROM specs WHERE tenant = ?1 AND entity_type = 'Issue'",
            params![tenant],
        )
        .await
        .expect("query spec");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("spec row exists");
    let version: i64 = row.get(0).expect("version");
    let verified: i64 = row.get(1).expect("verified");
    let status: String = row.get(2).expect("verification status");
    let committed: i64 = row.get(3).expect("committed");

    assert_eq!(version, 1, "identical spec commit must not bump version");
    assert_eq!(
        verified, 1,
        "identical spec commit must preserve verification"
    );
    assert_eq!(status, "passed");
    assert_eq!(committed, 1);
}

#[tokio::test]
async fn upsert_specs_and_commit_bypasses_write_gate_for_identical_app_specs() {
    let mut store = make_store("spec-idempotent-bypasses-gate").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";
    let policy = r#"permit(principal, action, resource);"#;

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            Some(policy),
            "test-app",
        )
        .await
        .expect("initial spec commit");

    store.write_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let held_gate = store
        .write_gate
        .clone()
        .acquire_owned()
        .await
        .expect("hold gate");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            Some(policy),
            "test-app",
        ),
    )
    .await;
    drop(held_gate);

    result
        .expect("identical app spec commit should bypass the write gate")
        .expect("identical app spec commit should succeed");
}

#[tokio::test]
async fn persist_spec_verification_keeps_updated_at_for_identical_result() {
    let store = make_store("spec-verification-idempotent").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";
    let result_json = r#"{"all_passed":true}"#;

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("initial spec commit");
    let update = TursoSpecVerificationUpdate {
        status: "passed",
        verified: true,
        levels_passed: Some(1),
        levels_total: Some(1),
        verification_result_json: Some(result_json),
    };
    store
        .persist_spec_verification(&tenant, "Issue", update)
        .await
        .expect("persist verification");

    let conn = store.connection().expect("connection");
    conn.execute(
        "UPDATE specs SET updated_at = 'fixed-time' WHERE tenant = ?1 AND entity_type = 'Issue'",
        params![tenant.as_str()],
    )
    .await
    .expect("pin updated_at");

    store
        .persist_spec_verification(&tenant, "Issue", update)
        .await
        .expect("persist identical verification");

    let mut rows = conn
        .query(
            "SELECT updated_at FROM specs WHERE tenant = ?1 AND entity_type = 'Issue'",
            params![tenant.as_str()],
        )
        .await
        .expect("query spec updated_at");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("spec row exists");
    let updated_at: String = row.get(0).expect("updated_at");

    assert_eq!(
        updated_at, "fixed-time",
        "identical verification persistence must not rewrite the spec row"
    );
}

#[tokio::test]
async fn persist_spec_verification_ignores_verified_at_only_changes() {
    let store = make_store("spec-verification-verified-at-idempotent").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";
    let first_result = r#"{"all_passed":true,"levels":[],"verified_at":"2026-04-28T17:00:00Z"}"#;
    let second_result = r#"{"all_passed":true,"levels":[],"verified_at":"2026-04-28T17:01:00Z"}"#;

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("initial spec commit");
    store
        .persist_spec_verification(
            &tenant,
            "Issue",
            TursoSpecVerificationUpdate {
                status: "passed",
                verified: true,
                levels_passed: Some(1),
                levels_total: Some(1),
                verification_result_json: Some(first_result),
            },
        )
        .await
        .expect("persist first verification");

    let conn = store.connection().expect("connection");
    conn.execute(
        "UPDATE specs SET updated_at = 'fixed-time' WHERE tenant = ?1 AND entity_type = 'Issue'",
        params![tenant.as_str()],
    )
    .await
    .expect("pin updated_at");

    store
        .persist_spec_verification(
            &tenant,
            "Issue",
            TursoSpecVerificationUpdate {
                status: "passed",
                verified: true,
                levels_passed: Some(1),
                levels_total: Some(1),
                verification_result_json: Some(second_result),
            },
        )
        .await
        .expect("persist timestamp-only verification change");

    let mut rows = conn
        .query(
            "SELECT updated_at, verification_result FROM specs WHERE tenant = ?1 AND entity_type = 'Issue'",
            params![tenant.as_str()],
        )
        .await
        .expect("query spec updated_at");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("spec row exists");
    let updated_at: String = row.get(0).expect("updated_at");
    let verification_result: String = row.get(1).expect("verification_result");

    assert_eq!(
        updated_at, "fixed-time",
        "verified_at-only verification changes must not rewrite the spec row"
    );
    assert_eq!(
        verification_result, first_result,
        "stored verification_result should remain stable when only verified_at changes"
    );
}

#[tokio::test]
async fn commit_specs_keeps_updated_at_when_specs_are_already_committed() {
    let store = make_store("spec-commit-idempotent").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("initial spec commit");

    let conn = store.connection().expect("connection");
    conn.execute(
        "UPDATE specs SET updated_at = 'fixed-time' WHERE tenant = ?1 AND entity_type = 'Issue'",
        params![tenant.as_str()],
    )
    .await
    .expect("pin updated_at");

    store
        .commit_specs(&tenant)
        .await
        .expect("commit already committed specs");

    let mut rows = conn
        .query(
            "SELECT updated_at FROM specs WHERE tenant = ?1 AND entity_type = 'Issue'",
            params![tenant.as_str()],
        )
        .await
        .expect("query spec updated_at");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("spec row exists");
    let updated_at: String = row.get(0).expect("updated_at");

    assert_eq!(
        updated_at, "fixed-time",
        "committing already committed specs must not rewrite them"
    );
}

#[tokio::test]
async fn load_verification_cache_ignores_uncommitted_specs() {
    let store = make_store("verification-cache-committed-only").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";

    store
        .upsert_spec(&tenant, "Issue", ioa_source, csdl_xml, content_hash)
        .await
        .expect("upsert uncommitted spec");
    store
        .persist_spec_verification(
            &tenant,
            "Issue",
            TursoSpecVerificationUpdate {
                status: "passed",
                verified: true,
                levels_passed: Some(1),
                levels_total: Some(1),
                verification_result_json: Some(r#"{"all_passed":true}"#),
            },
        )
        .await
        .expect("persist verification");

    let cache = store
        .load_verification_cache(&tenant)
        .await
        .expect("load verification cache");
    assert!(
        !cache.contains_key("Issue"),
        "uncommitted specs must not be used to skip bootstrap persistence"
    );

    store.commit_specs(&tenant).await.expect("commit spec");
    let cache = store
        .load_verification_cache(&tenant)
        .await
        .expect("load committed verification cache");
    assert_eq!(
        cache.get("Issue"),
        Some(&(content_hash.to_string(), true)),
        "committed verified specs should populate the verification cache"
    );
}

#[tokio::test]
async fn upsert_wasm_module_preserves_version_for_identical_hash() {
    let store = make_store("wasm-idempotent").await;

    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a", "bundled")
        .await
        .expect("initial wasm upsert");
    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a", "bundled")
        .await
        .expect("identical wasm upsert");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT version FROM wasm_modules WHERE tenant = ?1 AND module_name = ?2",
            params!["tenant-a", "mod-a"],
        )
        .await
        .expect("query wasm version");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("wasm row exists");
    let version: i64 = row.get(0).expect("version");

    assert_eq!(version, 1, "identical WASM hash must not bump version");
}

#[tokio::test]
async fn upsert_wasm_module_stores_metadata_only_without_db_blob() {
    let store = make_store("wasm-artifact").await;

    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a", "bundled")
        .await
        .expect("persist wasm artifact");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT length(wasm_bytes) FROM wasm_modules WHERE tenant = ?1 AND module_name = ?2",
            params!["tenant-a", "mod-a"],
        )
        .await
        .expect("query inline wasm length");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("wasm row exists");
    let inline_len: i64 = row.get(0).expect("inline wasm length");

    assert_eq!(
        inline_len, 0,
        "new WASM metadata rows should point at artifact storage, not inline bytes"
    );

    let artifact = store
        .get_blob("wasm-modules/hash-a")
        .await
        .expect("query legacy db blob");
    assert!(
        artifact.is_none(),
        "new WASM artifacts must not create Turso blob rows"
    );

    let loaded = store
        .load_wasm_module("tenant-a", "mod-a")
        .await
        .expect("load wasm row")
        .expect("wasm row exists");
    assert!(
        loaded.wasm_bytes.is_empty(),
        "Turso store should return metadata-only rows for new WASM artifacts"
    );
}
