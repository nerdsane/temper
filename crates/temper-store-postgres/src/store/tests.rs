//! PostgreSQL event-store regressions.

use super::*;
use crate::migration::run_migrations;
use temper_runtime::tenant::parse_persistence_id_parts;

fn parse_pid(persistence_id: &str) -> Result<(&str, &str, &str), PersistenceError> {
    parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)
}

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

// -- persistence_id parsing ---------------------------------------------

#[test]
fn parse_3_segment_persistence_id() {
    let (tenant, entity_type, entity_id) =
        parse_pid("alpha:Order:abc-123").expect("valid three-segment persistence ID");
    assert_eq!(tenant, "alpha");
    assert_eq!(entity_type, "Order");
    assert_eq!(entity_id, "abc-123");
}

#[test]
fn parse_legacy_2_segment_persistence_id() {
    let (tenant, entity_type, entity_id) =
        parse_pid("Order:abc-123").expect("valid legacy two-segment persistence ID");
    assert_eq!(tenant, "default");
    assert_eq!(entity_type, "Order");
    assert_eq!(entity_id, "abc-123");
}

#[test]
fn parse_3_segment_with_colons_in_id() {
    // splitn(3, ':') puts everything after the second colon into entity_id
    let (tenant, entity_type, entity_id) =
        parse_pid("beta:Task:T-1:sub").expect("valid persistence ID with colon in entity ID");
    assert_eq!(tenant, "beta");
    assert_eq!(entity_type, "Task");
    assert_eq!(entity_id, "T-1:sub");
}

#[test]
fn parse_persistence_id_missing_colon() {
    let err = parse_pid("OrderAbc123").unwrap_err();
    assert!(
        matches!(err, PersistenceError::Storage(_)),
        "expected Storage error, got: {err:?}"
    );
}

#[test]
fn parse_persistence_id_empty_segment() {
    assert!(parse_pid(":Order:abc").is_err());
    assert!(parse_pid("tenant::abc").is_err());
    assert!(parse_pid("tenant:Order:").is_err());
    assert!(parse_pid(":abc").is_err());
    assert!(parse_pid("Order:").is_err());
}

#[test]
fn list_entity_ids_returns_distinct_pairs() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping Postgres integration test: DATABASE_URL is not set");
            return;
        }
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url)
            .await
            .expect("connect to DATABASE_URL");
        run_migrations(&pool).await.expect("run migrations");
        let store = PostgresEventStore::new(pool);

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
                    serde_json::json!({"id":"ord-1"}),
                )],
            )
            .await
            .expect("append first order event");
        store
            .append(
                &order_1,
                1,
                &[test_envelope(
                    "OrderUpdated",
                    serde_json::json!({"step": 2}),
                )],
            )
            .await
            .expect("append order update event");
        store
            .append(
                &order_2,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({"id":"ord-2"}),
                )],
            )
            .await
            .expect("append second order event");
        store
            .append(
                &task_1,
                0,
                &[test_envelope(
                    "TaskCreated",
                    serde_json::json!({"id":"task-1"}),
                )],
            )
            .await
            .expect("append task event");
        store
            .append(
                &other_tenant,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({"id":"ord-9"}),
                )],
            )
            .await
            .expect("append other tenant event");

        let mut entities = store
            .list_entity_ids(&tenant_a)
            .await
            .expect("list tenant entity IDs");
        entities.sort();

        assert_eq!(
            entities,
            vec![
                ("Order".to_string(), "ord-1".to_string()),
                ("Order".to_string(), "ord-2".to_string()),
                ("Task".to_string(), "task-1".to_string()),
            ]
        );
    });
}

#[test]
fn postgres_platform_methods_are_part_of_the_store_surface() {
    // Compile-only check: the function body is never executed, so the
    // unawaited futures are intentional. Clippy's let_underscore_future
    // lint catches forgotten awaits in real code; here it's a false
    // positive.
    #[expect(
        clippy::let_underscore_future,
        reason = "compile-only API coverage intentionally constructs but never polls futures"
    )]
    fn assert_methods(store: &PostgresEventStore) {
        let _ = store.upsert_query_projection(
            "tenant",
            "Session",
            "s-1",
            "Running",
            &serde_json::json!({"phase":"Running"}),
            1,
        );
        let _ = store.remove_query_projection("tenant", "Session", "s-1");
        let _ = store.query_field_index(
            "tenant",
            "Session",
            "fields @> $3::jsonb",
            vec!["{\"phase\":\"Running\"}".to_string()],
        );
        let _ = store.persist_trajectory(crate::PostgresTrajectoryInsert {
            tenant: "tenant",
            entity_type: "Session",
            entity_id: "s-1",
            action: "ProgressMade",
            success: true,
            from_status: Some("Running"),
            to_status: Some("Running"),
            error: None,
            agent_id: Some("agent"),
            session_id: Some("session"),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some("Entity"),
            spec_governed: Some(true),
            created_at: "2026-04-28T00:00:00Z",
            request_body: Some("{\"ok\":true}"),
            intent: Some("test"),
            matched_policy_ids: Some("[\"policy:test\"]"),
        });
        let _ = store.save_policy(
            "tenant",
            "primary",
            "permit(principal, action, resource);",
            "test",
        );
        let _ = store.load_policies_for_tenant("tenant");
        let _ = store.load_all_policies();
        let _ = store.toggle_policy_enabled("tenant", "primary", true);
        let _ = store.update_policy_text(
            "tenant",
            "primary",
            "permit(principal, action, resource);",
            "test",
        );
        let _ = store.delete_policy("tenant", "primary");
    }

    let _ = assert_methods;
}

#[test]
fn postgres_long_tail_methods_are_part_of_the_store_surface() {
    let _ = PostgresEventStore::load_recent_trajectories;
    let _ = PostgresEventStore::load_unmet_intent_rows;
    let _ = PostgresEventStore::load_submit_spec_timestamps;
    let _ = PostgresEventStore::count_trajectories_by_tenant;
    let _ = PostgresEventStore::query_trajectory_stats;
    let _ = PostgresEventStore::query_trajectories_by_agent;
    let _ = PostgresEventStore::query_agent_summaries;

    let _ = PostgresEventStore::upsert_feature_request;
    let _ = PostgresEventStore::list_feature_requests;
    let _ = PostgresEventStore::update_feature_request;
    let _ = PostgresEventStore::insert_evolution_record;
    let _ = PostgresEventStore::get_evolution_record;
    let _ = PostgresEventStore::list_evolution_records;
    let _ = PostgresEventStore::list_ranked_insights;
    let _ = PostgresEventStore::insert_design_time_event;
    let _ = PostgresEventStore::list_design_time_events;

    let _ = PostgresEventStore::persist_ots_trajectory;
    let _ = PostgresEventStore::list_ots_trajectories;
    let _ = PostgresEventStore::get_ots_trajectory;

    let _ = PostgresEventStore::put_blob;
    let _ = PostgresEventStore::put_blob_with_ttl;
    let _ = PostgresEventStore::get_blob;
    let _ = PostgresEventStore::sweep_expired_blobs;

    let _ = PostgresEventStore::upsert_secret;
    let _ = PostgresEventStore::delete_secret;
    let _ = PostgresEventStore::load_secrets_for_tenant;

    let _ = PostgresEventStore::upsert_policy_denial_pattern;
    let _ = PostgresEventStore::load_policy_denial_patterns;

    let _ = PostgresEventStore::query_decisions;
    let _ = PostgresEventStore::query_all_decisions;
    let _ = PostgresEventStore::get_pending_decision;

    let _ = PostgresEventStore::load_wasm_module;
    let _ = PostgresEventStore::load_wasm_module_metadata_all_tenants;
    let _ = PostgresEventStore::persist_wasm_invocation;
    let _ = PostgresEventStore::load_recent_wasm_invocations;
    let _ = PostgresEventStore::delete_wasm_module;

    let _ = PostgresEventStore::upsert_published_artifact;
    let _ = PostgresEventStore::load_published_artifact;
}

#[test]
fn postgres_query_projection_batch_method_is_part_of_the_store_surface() {
    let _ = PostgresEventStore::load_query_projection_fields_many;
    let _ = PostgresEventStore::load_selected_entity_catalog_rows_pg;
}

#[test]
fn load_query_projection_fields_many_returns_requested_fields() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping Postgres integration test: DATABASE_URL is not set");
            return;
        }
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url)
            .await
            .expect("connect to DATABASE_URL");
        run_migrations(&pool).await.expect("run migrations");
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-projection-{}", uuid::Uuid::new_v4());

        store
            .upsert_query_projection(
                &tenant,
                "File",
                "file-a",
                "Ready",
                &serde_json::json!({
                    "content_hash": "sha256:file-a",
                    "mime_type": "text/plain",
                    "has_content": true,
                }),
                12,
            )
            .await
            .expect("upsert query projection row");

        let rows = store
            .load_query_projection_fields_many(
                &tenant,
                "File",
                &["file-a".to_string(), "missing".to_string()],
                &["content_hash", "has_content"],
            )
            .await
            .expect("load requested projection fields");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity_id, "file-a");
        assert_eq!(rows[0].status, "Ready");
        assert_eq!(
            rows[0]
                .fields
                .get("content_hash")
                .and_then(|value| value.as_deref()),
            Some("sha256:file-a")
        );
        assert_eq!(
            rows[0]
                .fields
                .get("has_content")
                .and_then(|value| value.as_deref()),
            Some("true")
        );

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .expect("delete test entity field index rows");
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .expect("delete test entity catalog rows");
    });
}

#[test]
fn published_artifact_upsert_round_trips_and_updates_by_id() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url)
            .await
            .expect("connect to DATABASE_URL");
        run_migrations(&pool).await.expect("run migrations");
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-published-artifact-{}", uuid::Uuid::new_v4());

        let artifact = crate::PostgresPublishedArtifactUpsert {
            id: "part-test",
            tenant: &tenant,
            source_file_id: "file-a",
            source_file_version_id: "",
            content_hash: "sha256:abc",
            label: "latest",
            mime_type: "text/markdown",
            byte_length: 42,
            public_storage_key: "published/file-a.md",
            public_url: "https://assets.example/published/file-a.md",
            owner_ref_type: "Doc",
            owner_ref_id: "doc-a",
            status: "published",
        };

        let row = store
            .upsert_published_artifact(&artifact)
            .await
            .expect("upsert published artifact");
        assert_eq!(row.tenant, tenant);
        assert_eq!(row.id, "part-test");
        assert_eq!(row.public_url, "https://assets.example/published/file-a.md");

        let updated = crate::PostgresPublishedArtifactUpsert {
            public_url: "https://assets.example/published/file-a-v2.md",
            public_storage_key: "published/file-a-v2.md",
            byte_length: 43,
            ..artifact
        };
        store
            .upsert_published_artifact(&updated)
            .await
            .expect("update published artifact");

        let loaded = store
            .load_published_artifact(&tenant, "part-test")
            .await
            .expect("load published artifact")
            .expect("artifact should load after upsert");

        assert_eq!(
            loaded.public_url,
            "https://assets.example/published/file-a-v2.md"
        );
        assert_eq!(loaded.public_storage_key, "published/file-a-v2.md");
        assert_eq!(loaded.byte_length, 43);

        crate::dbm::postgres_query!("DELETE FROM published_artifacts WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .expect("delete test published artifact rows");
    });
}
