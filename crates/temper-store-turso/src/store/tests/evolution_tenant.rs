use crate::TursoTrajectoryInsert;

use super::{make_store, sqlite_test_url};

#[tokio::test]
async fn legacy_evolution_tables_gain_default_tenant_ownership() {
    let url = sqlite_test_url("evolution-legacy-migration");
    let path = url.strip_prefix("file:").unwrap();
    let database = libsql::Builder::new_local(path).build().await.unwrap();
    let connection = database.connect().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE feature_requests (\
                id TEXT PRIMARY KEY, category TEXT NOT NULL, description TEXT NOT NULL, \
                frequency INTEGER NOT NULL DEFAULT 0, trajectory_refs TEXT NOT NULL DEFAULT '[]', \
                disposition TEXT NOT NULL DEFAULT 'Open', developer_notes TEXT, \
                created_at TEXT NOT NULL DEFAULT (datetime('now')), \
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))\
             ); \
             CREATE TABLE evolution_records (\
                id TEXT PRIMARY KEY, record_type TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'Open', \
                created_by TEXT NOT NULL, derived_from TEXT, data TEXT NOT NULL, \
                timestamp TEXT NOT NULL DEFAULT (datetime('now'))\
             ); \
             INSERT INTO feature_requests (id, category, description) \
                VALUES ('legacy-feature', 'Workflow', 'legacy'); \
             INSERT INTO evolution_records (id, record_type, created_by, data) \
                VALUES ('O-legacy', 'Observation', 'legacy', '{}');",
        )
        .await
        .unwrap();
    drop(connection);
    drop(database);

    let store = crate::TursoEventStore::new(&url, None).await.unwrap();
    let features = store.list_feature_requests("default", None).await.unwrap();
    assert_eq!(features[0].id, "legacy-feature");
    assert_eq!(features[0].tenant, "default");
    let records = store
        .list_evolution_records("default", None, None)
        .await
        .unwrap();
    assert_eq!(records[0].id, "O-legacy");
    assert_eq!(records[0].tenant, "default");
}

#[tokio::test]
async fn feature_requests_and_evolution_records_are_tenant_owned() {
    let store = make_store("evolution-tenant-owned").await;
    store
        .upsert_feature_request(
            "tenant-a",
            "feature-a",
            "Workflow",
            "tenant A request",
            4,
            "[]",
            "Open",
            None,
        )
        .await
        .unwrap();
    store
        .upsert_feature_request(
            "tenant-b",
            "feature-b",
            "Workflow",
            "tenant B request",
            9,
            "[]",
            "Open",
            None,
        )
        .await
        .unwrap();

    let tenant_a_features = store.list_feature_requests("tenant-a", None).await.unwrap();
    assert_eq!(tenant_a_features.len(), 1);
    assert_eq!(tenant_a_features[0].id, "feature-a");
    assert_eq!(tenant_a_features[0].tenant, "tenant-a");
    assert!(
        !store
            .update_feature_request("tenant-a", "feature-b", "Resolved", None)
            .await
            .unwrap()
    );

    store
        .insert_evolution_record(crate::TursoEvolutionRecordInsert {
            tenant: "tenant-a",
            id: "O-a",
            record_type: "Observation",
            status: "Open",
            created_by: "test",
            derived_from: None,
            data_json: "{}",
        })
        .await
        .unwrap();
    store
        .insert_evolution_record(crate::TursoEvolutionRecordInsert {
            tenant: "tenant-b",
            id: "O-b",
            record_type: "Observation",
            status: "Open",
            created_by: "test",
            derived_from: None,
            data_json: "{}",
        })
        .await
        .unwrap();

    assert!(
        store
            .get_evolution_record("tenant-a", "O-b")
            .await
            .unwrap()
            .is_none()
    );
    let tenant_a_records = store
        .list_evolution_records("tenant-a", None, None)
        .await
        .unwrap();
    assert_eq!(tenant_a_records.len(), 1);
    assert_eq!(tenant_a_records[0].id, "O-a");
    assert_eq!(tenant_a_records[0].tenant, "tenant-a");
}

#[tokio::test]
async fn trajectory_tenant_predicate_is_applied_before_limit() {
    let store = make_store("trajectory-tenant-limit").await;
    for (tenant, entity_id, created_at) in [
        ("tenant-a", "a-old", "2026-01-01T00:00:00Z"),
        ("tenant-b", "b-new", "2026-02-01T00:00:00Z"),
    ] {
        store
            .persist_trajectory(TursoTrajectoryInsert {
                tenant,
                entity_type: "Order",
                entity_id,
                action: "Submit",
                success: false,
                from_status: None,
                to_status: None,
                error: Some("unmet"),
                agent_id: None,
                session_id: None,
                authz_denied: Some(false),
                denied_resource: None,
                denied_module: None,
                source: Some("Entity"),
                spec_governed: Some(true),
                created_at,
                request_body: None,
                intent: None,
                matched_policy_ids: None,
            })
            .await
            .unwrap();
    }

    let rows = store.load_recent_trajectories("tenant-a", 1).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entity_id, "a-old");
    assert_eq!(rows[0].tenant, "tenant-a");
    let unmet = store.load_unmet_intent_rows("tenant-a").await.unwrap();
    assert_eq!(unmet.len(), 1);
    assert_eq!(unmet[0].count, 1);
}
