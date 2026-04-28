//! Integration tests for the Turso event store.

use libsql::params;
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, PersistenceError,
};

use super::TursoEventStore;
use crate::TursoSpecVerificationUpdate;

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
async fn list_entity_ids_by_type_falls_back_to_events_and_excludes_deleted() {
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
        .expect("list Order IDs by type from event fallback");

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
async fn load_wasm_module_metadata_all_tenants_returns_metadata_without_bulk_bytes() {
    let store = make_store("wasm-metadata").await;

    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a")
        .await
        .expect("persist mod-a");
    store
        .upsert_wasm_module("tenant-b", "mod-b", b"hello-b", "hash-b")
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
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a")
        .await
        .expect("initial wasm upsert");
    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a")
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
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a")
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
