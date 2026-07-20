//! Postgres event-store unit and live regressions.

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
fn snapshot_replacement_rejects_stale_same_boundary_writer() {
    type SegmentShape = (i64, i64, Option<i64>, Option<i64>, i64);

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
        let tenant = format!("tenant-snapshot-cas-{}", uuid::Uuid::new_v4());
        let persistence_id = format!("{tenant}:Order:concurrent-repair");

        store
            .append(
                &persistence_id,
                0,
                &[
                    test_envelope("Created", serde_json::json!({})),
                    test_envelope("Updated", serde_json::json!({})),
                ],
            )
            .await
            .expect("seed snapshot boundary events");
        let read = store
            .read_events_with_head(&persistence_id, 1)
            .await
            .expect("read tail and durable journal head");
        assert_eq!(read.journal_head_sequence_nr, 2);
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].sequence_nr, 2);
        store
            .save_snapshot(&persistence_id, 2, b"legacy-snapshot")
            .await
            .expect("seed legacy boundary");
        let segments_before: Vec<SegmentShape> =
                crate::dbm::postgres_query_as!(
                    "SELECT segment_index, start_sequence_nr, end_sequence_nr, \
                            snapshot_sequence, event_count \
                     FROM event_segments \
                     WHERE tenant = $1 AND entity_type = 'Order' AND entity_id = 'concurrent-repair' \
                     ORDER BY segment_index",
                )
                .bind(&tenant)
                .fetch_all(&pool)
                .await
                .expect("read segment metadata before replacement");
        store
            .replace_snapshot(&persistence_id, 2, b"legacy-snapshot", b"first-repair")
            .await
            .expect("first repair claims the legacy boundary");
        let stale_repair = store
            .replace_snapshot(
                &persistence_id,
                2,
                b"legacy-snapshot",
                b"stale-second-repair",
            )
            .await
            .expect_err("a stale same-boundary writer must lose");

        assert!(matches!(stale_repair, PersistenceError::Storage(_)));
        assert_eq!(
            store.load_snapshot(&persistence_id).await.unwrap(),
            Some((2, b"first-repair".to_vec())),
            "the winning repair must not be overwritten"
        );
        let history: (Vec<u8>,) = crate::dbm::postgres_query_as!(
            "SELECT state FROM snapshot_history \
                 WHERE tenant = $1 AND entity_type = 'Order' \
                   AND entity_id = 'concurrent-repair' AND sequence_nr = 2",
        )
        .bind(&tenant)
        .fetch_one(&pool)
        .await
        .expect("read replacement history payload");
        assert_eq!(history.0, b"first-repair");
        let segments_after: Vec<SegmentShape> =
                crate::dbm::postgres_query_as!(
                    "SELECT segment_index, start_sequence_nr, end_sequence_nr, \
                            snapshot_sequence, event_count \
                     FROM event_segments \
                     WHERE tenant = $1 AND entity_type = 'Order' AND entity_id = 'concurrent-repair' \
                     ORDER BY segment_index",
                )
                .bind(&tenant)
                .fetch_all(&pool)
                .await
                .expect("read segment metadata after replacement");
        assert_eq!(segments_after, segments_before);

        crate::dbm::postgres_query!("DELETE FROM snapshots WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .expect("delete test snapshot");
        crate::dbm::postgres_query!("DELETE FROM snapshot_history WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .expect("delete test snapshot history");
        crate::dbm::postgres_query!("DELETE FROM event_segments WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .expect("delete test event segments");
        crate::dbm::postgres_query!("DELETE FROM events WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .expect("delete test events");
    });
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
    #[allow(clippy::let_underscore_future)]
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

#[path = "projection.rs"]
mod projection;
