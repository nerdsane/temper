use super::*;
use crate::migration::run_migrations;
use sqlx::PgPool;
use temper_runtime::persistence::{
    EntityKeyRow, EventStore, PersistenceAppend, PersistenceError, PersistenceSequenceGuard,
};

type SegmentRow = (i64, i64, Option<i64>, Option<i64>, i64, bool);

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
            actor_id: "store-projection-test".to_string(),
        },
    }
}

/// ARN-192 contract proof for the real PostgreSQL tail query. Gated on
/// `DATABASE_URL`; preserves input order and duplicates, returns `None` for a
/// missing stream, and decodes only the newest row.
#[test]
#[ignore = "requires DATABASE_URL and a live PostgreSQL service"]
fn latest_events_preserve_order_duplicates_missing_and_tail_on_postgres() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL for ignored PostgreSQL integration test");

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-latest-{}", uuid::Uuid::new_v4());
        let first = format!("{tenant}:Order:first");
        let second = format!("{tenant}:Order:second");
        let missing = format!("{tenant}:Order:missing");

        store
            .append(
                &first,
                0,
                &[
                    test_envelope("Created", serde_json::json!({"version": 1})),
                    test_envelope("Updated", serde_json::json!({"version": 2})),
                ],
            )
            .await
            .unwrap();
        store
            .append(
                &second,
                0,
                &[test_envelope("Created", serde_json::json!({"version": 1}))],
            )
            .await
            .unwrap();

        let latest = store
            .read_latest_events(&[second.clone(), missing, first.clone(), first.clone()])
            .await
            .unwrap();
        assert_eq!(latest.len(), 4);
        assert_eq!(latest[0].as_ref().unwrap().sequence_nr, 1);
        assert!(latest[1].is_none());
        assert_eq!(latest[2].as_ref().unwrap().sequence_nr, 2);
        assert_eq!(latest[2].as_ref().unwrap().payload["version"], 2);
        assert_eq!(latest[3].as_ref().unwrap().sequence_nr, 2);

        let bounded = store.read_events_bounded(&first, 0, 1).await.unwrap();
        assert_eq!(bounded.len(), 1);
        assert_eq!(bounded[0].sequence_nr, 1);
        assert!(
            store
                .read_events_bounded(&first, 0, 0)
                .await
                .unwrap()
                .is_empty()
        );

        crate::dbm::postgres_query!(
            "UPDATE events SET metadata = '{}'::jsonb \
             WHERE tenant = $1 AND entity_type = 'Order' AND entity_id = 'first' \
               AND sequence_nr = 2",
        )
        .bind(&tenant)
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            store.read_latest_events(std::slice::from_ref(&first)).await,
            Err(PersistenceError::Serialization(_))
        ));

        crate::dbm::postgres_query!("DELETE FROM events WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
#[ignore = "requires DATABASE_URL and a live PostgreSQL service"]
fn delayed_snapshot_preserves_postgres_recovery_and_segment_order() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL for ignored PostgreSQL integration test");

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-snapshot-{}", uuid::Uuid::new_v4());
        let snapshot_only = format!("{tenant}:Order:snapshot-only");
        store
            .save_snapshot(&snapshot_only, 5, b"snapshot-only")
            .await
            .unwrap();
        let invented_segments: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COUNT(*)::bigint FROM event_segments \
             WHERE tenant = $1 AND entity_type = 'Order' AND entity_id = 'snapshot-only'",
        )
        .bind(&tenant)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(invented_segments, 0);

        let persistence_id = format!("{tenant}:Order:ordered");
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
            .unwrap();
        store
            .save_snapshot(&persistence_id, 2, b"snapshot-2")
            .await
            .unwrap();
        store
            .append(
                &persistence_id,
                2,
                &[test_envelope("UpdatedAgain", serde_json::json!({}))],
            )
            .await
            .unwrap();
        store
            .save_snapshot(&persistence_id, 2, b"snapshot-2-delayed")
            .await
            .unwrap();
        store
            .save_snapshot(&persistence_id, 3, b"snapshot-3")
            .await
            .unwrap();
        store
            .save_snapshot(&persistence_id, 1, b"snapshot-1-stale")
            .await
            .unwrap();

        assert_eq!(
            store.load_snapshot(&persistence_id).await.unwrap(),
            Some((3, b"snapshot-3".to_vec()))
        );
        let segments: Vec<SegmentRow> = crate::dbm::postgres_query_as!(
            "SELECT segment_index, start_sequence_nr, end_sequence_nr, \
                        snapshot_sequence, event_count, (sealed_at IS NOT NULL) \
                 FROM event_segments \
                 WHERE tenant = $1 AND entity_type = 'Order' AND entity_id = 'ordered' \
                 ORDER BY segment_index",
        )
        .bind(&tenant)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            segments,
            vec![
                (0, 1, Some(2), Some(2), 2, true),
                (1, 3, Some(3), Some(3), 1, true),
                (2, 4, None, None, 0, false),
            ]
        );

        for table in ["snapshot_history", "snapshots", "event_segments", "events"] {
            let query = format!("DELETE FROM {table} WHERE tenant = $1");
            sqlx::query(&query)
                .bind(&tenant)
                .execute(&pool)
                .await
                .unwrap();
        }
    });
}

#[test]
fn unconditional_projection_removal_clears_orphan_field_rows() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-orphan-{}", uuid::Uuid::new_v4());
        let entity_type = "Order";
        let entity_id = "ord-orphan";
        crate::dbm::postgres_query!(
            "INSERT INTO entity_field_index \
             (tenant, entity_type, entity_id, field_name, field_value, status) \
             VALUES ($1, $2, $3, 'Title', 'orphan', 'Draft')",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(&pool)
        .await
        .unwrap();

        store
            .remove_query_projection(&tenant, entity_type, entity_id)
            .await
            .unwrap();

        let remaining: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COUNT(*)::bigint FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(remaining, 0);
    });
}

#[test]
fn guarded_append_rejects_stale_context_without_writing_target() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-guarded-{}", uuid::Uuid::new_v4());
        let context_id = format!("{tenant}:Owner:owner-guard");
        let target_id = format!("{tenant}:Document:document-guard");
        store
            .append(
                &context_id,
                0,
                &[test_envelope("Created", serde_json::json!({}))],
            )
            .await
            .unwrap();
        let error = store
            .append_batch_guarded(
                &[PersistenceAppend {
                    persistence_id: target_id.clone(),
                    expected_sequence: 0,
                    events: vec![test_envelope("FieldsPatched", serde_json::json!({}))],
                    key_rows: None,
                    vector_rows: Vec::new(),
                    reconcile_vectors: false,
                }],
                &[PersistenceSequenceGuard {
                    persistence_id: context_id.clone(),
                    expected_sequence: 0,
                }],
            )
            .await
            .expect_err("stale PostgreSQL guard must abort target append");
        assert!(matches!(error, PersistenceError::PreconditionFailed { .. }));
        assert!(store.read_events(&target_id, 0).await.unwrap().is_empty());
        let result = store
            .append_batch_guarded(
                &[PersistenceAppend {
                    persistence_id: target_id.clone(),
                    expected_sequence: 0,
                    events: vec![test_envelope("FieldsPatched", serde_json::json!({}))],
                    key_rows: None,
                    vector_rows: Vec::new(),
                    reconcile_vectors: false,
                }],
                &[PersistenceSequenceGuard {
                    persistence_id: context_id,
                    expected_sequence: 1,
                }],
            )
            .await
            .expect("current PostgreSQL guard should commit target atomically");
        assert_eq!(result[0].sequence_nr, 1);
        assert_eq!(store.read_events(&target_id, 0).await.unwrap().len(), 1);
        crate::dbm::postgres_query!("DELETE FROM events WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

/// ADR-0153 live verification: the real postgres store honors the same
/// negative-existence + atomicity invariants the DST proved in SimEventStore.
/// Gated on DATABASE_URL (skips otherwise); isolated by a unique tenant.
#[test]
fn entity_key_index_present_absent_and_atomic_reject() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-keyindex-{}", uuid::Uuid::new_v4());
        let key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: format!("kh-{}", uuid::Uuid::new_v4()),
        };

        // A claims the key (co-committed with the journal append).
        let pid_a = format!("{tenant}:Doc:doc-a");
        store
            .append_with_keys(
                &pid_a,
                0,
                &[test_envelope("Create", serde_json::json!({}))],
                std::slice::from_ref(&key),
            )
            .await
            .unwrap();

        // PRESENT and ABSENT in one keyed probe.
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", &key.key_hash)
                .await
                .unwrap(),
            Some("doc-a".to_string()),
        );
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", "no-such-hash")
                .await
                .unwrap(),
            None,
        );

        // ATOMICITY: B claims the SAME key -> rejected, and B's journal is unchanged.
        let pid_b = format!("{tenant}:Doc:doc-b");
        let res = store
            .append_with_keys(
                &pid_b,
                0,
                &[test_envelope("Create", serde_json::json!({}))],
                std::slice::from_ref(&key),
            )
            .await;
        assert!(res.is_err(), "duplicate declared key must be rejected");
        assert!(
            store.read_events(&pid_b, 0).await.unwrap().is_empty(),
            "a rejected co-commit must leave the journal unchanged (atomic)"
        );
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", &key.key_hash)
                .await
                .unwrap(),
            Some("doc-a".to_string()),
        );

        // A's tombstone retires every key claim in the SAME transaction, so a
        // later generation/entity can safely reuse the declared key.
        store
            .append_batch(&[PersistenceAppend {
                persistence_id: pid_a.clone(),
                expected_sequence: 1,
                events: vec![
                    test_envelope("Deleted", serde_json::json!({})),
                    test_envelope(
                        temper_runtime::persistence::COMPOSITE_EVENT_TYPE,
                        serde_json::json!({}),
                    ),
                ],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            }])
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", &key.key_hash)
                .await
                .unwrap(),
            None,
        );
        store
            .append_with_keys(
                &pid_b,
                0,
                &[test_envelope("Create", serde_json::json!({}))],
                std::slice::from_ref(&key),
            )
            .await
            .expect("deleted key can be reused by another entity");
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", &key.key_hash)
                .await
                .unwrap(),
            Some("doc-b".to_string()),
        );

        // A delayed cleanup for A's first deletion generation must not erase
        // a key claimed by A after a valid recreation at sequence 4.
        let recreated_key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: format!("kh-recreated-{}", uuid::Uuid::new_v4()),
        };
        store
            .append_with_keys(
                &pid_a,
                3,
                &[test_envelope("Created", serde_json::json!({}))],
                std::slice::from_ref(&recreated_key),
            )
            .await
            .expect("recreate deleted stream");
        store
            .retire_entity_keys_through_sequence(&tenant, "Doc", "doc-a", 3)
            .await
            .expect("run delayed cleanup for old generation");
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", &recreated_key.key_hash)
                .await
                .unwrap(),
            Some("doc-a".to_string()),
        );

        // Legacy rows that survived a terminal tombstone are still repaired
        // when there is no later Created generation.
        let legacy_pid = format!("{tenant}:Doc:doc-legacy");
        let legacy_key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: format!("kh-legacy-{}", uuid::Uuid::new_v4()),
        };
        store
            .append(
                &legacy_pid,
                0,
                &[
                    test_envelope("Created", serde_json::json!({})),
                    test_envelope("Deleted", serde_json::json!({})),
                ],
            )
            .await
            .unwrap();
        store
            .backfill_entity_keys(
                &tenant,
                "Doc",
                "doc-legacy",
                std::slice::from_ref(&legacy_key),
            )
            .await
            .expect("seed a legacy stale key row");
        store
            .retire_entity_keys_through_sequence(&tenant, "Doc", "doc-legacy", 2)
            .await
            .expect("retire the legacy stale key row");
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", &legacy_key.key_hash)
                .await
                .unwrap(),
            None,
        );

        let empty_pid = format!("{tenant}:Doc:empty-batch");
        let empty_result = store
            .append_batch(&[PersistenceAppend {
                persistence_id: empty_pid.clone(),
                expected_sequence: 7,
                events: vec![],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            }])
            .await
            .unwrap();
        assert_eq!(empty_result[0].sequence_nr, 7);
        assert!(store.read_events(&empty_pid, 0).await.unwrap().is_empty());
        assert!(
            !store
                .list_entity_ids(&tenant)
                .await
                .unwrap()
                .iter()
                .any(|(_, entity_id)| entity_id == "empty-batch")
        );
        let segment_count: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COUNT(*) FROM event_segments \
             WHERE tenant = $1 AND entity_type = 'Doc' AND entity_id = 'empty-batch'",
        )
        .bind(&tenant)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(segment_count, 0);

        let raw_pid = format!("{tenant}:Doc:doc-raw-key");
        let raw_key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: format!("kh-raw-{}", uuid::Uuid::new_v4()),
        };
        store
            .append_with_keys(
                &raw_pid,
                0,
                &[test_envelope("Created", serde_json::json!({}))],
                std::slice::from_ref(&raw_key),
            )
            .await
            .unwrap();
        store
            .append_batch(&[PersistenceAppend {
                persistence_id: raw_pid.clone(),
                expected_sequence: 1,
                events: vec![test_envelope("ExternalAudit", serde_json::json!({}))],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            }])
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", &raw_key.key_hash)
                .await
                .unwrap(),
            Some("doc-raw-key".to_string()),
            "raw batch append must preserve key claims it cannot recompute"
        );
        store
            .append_batch(&[PersistenceAppend {
                persistence_id: raw_pid.clone(),
                expected_sequence: 2,
                events: vec![test_envelope("KeyRemoved", serde_json::json!({}))],
                key_rows: Some(Vec::new()),
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            }])
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", &raw_key.key_hash)
                .await
                .unwrap(),
            None,
            "authoritative empty batch replacement must clear prior claims"
        );

        // Clean up this test tenant's rows.
        let _ = crate::dbm::postgres_query!("DELETE FROM entity_key_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await;
        let _ = crate::dbm::postgres_query!("DELETE FROM events WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await;
    });
}

/// ADR-0153 backfill robustness: the real postgres store honors the backfill
/// primitives the resumable, watermark-gated backfill relies on —
/// `backfill_entity_keys` (no journal event), `keyed_entity_ids_for_type` (resume:
/// which entities are already keyed), and the watermark round-trip
/// (`mark_key_index_backfilled` / `key_index_backfilled_types`, table from migration
/// 0010). Gated on DATABASE_URL; isolated by a unique tenant.
#[test]
fn key_index_backfill_and_watermark_methods_round_trip_on_postgres() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-backfill-{}", uuid::Uuid::new_v4());
        let key = EntityKeyRow {
            key_name: "name_parent".to_string(),
            key_hash: format!("root-{}", uuid::Uuid::new_v4()),
        };

        // Backfill (no journal event) keys a pre-existing entity — the root case.
        store
            .backfill_entity_keys(&tenant, "Directory", "dir-root", std::slice::from_ref(&key))
            .await
            .unwrap();

        // Resumability source: the entity now shows as already-keyed for the type.
        assert_eq!(
            store
                .keyed_entity_ids_for_type(&tenant, "Directory")
                .await
                .unwrap(),
            vec!["dir-root".to_string()],
        );
        // And it resolves by the declared key.
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Directory", "name_parent", &key.key_hash)
                .await
                .unwrap(),
            Some("dir-root".to_string()),
        );

        // Watermark round-trip: not set until marked, then present for that type only.
        assert!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .unwrap()
                .is_empty()
        );
        store
            .mark_key_index_backfilled(&tenant, "Directory", "name_parent")
            .await
            .unwrap();
        assert_eq!(
            store.key_index_backfilled_types(&tenant).await.unwrap(),
            vec![("Directory".to_string(), "name_parent".to_string())],
        );
        // Idempotent for the same key-set.
        store
            .mark_key_index_backfilled(&tenant, "Directory", "name_parent")
            .await
            .unwrap();
        assert_eq!(
            store.key_index_backfilled_types(&tenant).await.unwrap(),
            vec![("Directory".to_string(), "name_parent".to_string())],
        );
        // A re-mark with a CHANGED key-set OVERWRITES the recorded set (ARN-68: adding a
        // key must move the watermark to the new declaration, not DO NOTHING).
        store
            .mark_key_index_backfilled(&tenant, "Directory", "name_parent,ws_path")
            .await
            .unwrap();
        assert_eq!(
            store.key_index_backfilled_types(&tenant).await.unwrap(),
            vec![("Directory".to_string(), "name_parent,ws_path".to_string())],
        );

        let _ = crate::dbm::postgres_query!("DELETE FROM entity_key_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await;
        let _ = crate::dbm::postgres_query!(
            "DELETE FROM key_index_backfill_watermark WHERE tenant = $1"
        )
        .bind(&tenant)
        .execute(&pool)
        .await;
    });
}

#[test]
fn list_entity_ids_by_type_uses_only_authoritative_event_streams() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-projection-list-{}", uuid::Uuid::new_v4());
        let entity_type = "DesignLanguage";

        store
            .upsert_query_projection(
                &tenant,
                entity_type,
                "dl-catalog",
                "Published",
                &serde_json::json!({ "Name": "Catalog" }),
                1,
            )
            .await
            .unwrap();
        store
            .upsert_query_projection(
                &tenant,
                entity_type,
                "dl-deleted",
                "Published",
                &serde_json::json!({ "Name": "Deleted" }),
                1,
            )
            .await
            .unwrap();
        crate::dbm::postgres_query!(
            "INSERT INTO entity_field_index \
             (tenant, entity_type, entity_id, field_name, field_value, status) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind("dl-index")
        .bind("Name")
        .bind("Index")
        .bind("Published")
        .execute(&pool)
        .await
        .unwrap();
        store
            .append(
                &format!("{tenant}:{entity_type}:dl-event"),
                0,
                &[test_envelope("Created", serde_json::json!({}))],
            )
            .await
            .unwrap();
        store
            .append(
                &format!("{tenant}:{entity_type}:dl-deleted"),
                0,
                &[test_envelope("Deleted", serde_json::json!({}))],
            )
            .await
            .unwrap();

        let ids = store
            .list_entity_ids_by_type(&tenant, entity_type)
            .await
            .unwrap();

        assert_eq!(ids, vec!["dl-deleted".to_string(), "dl-event".to_string(),]);

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM events WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
fn load_selected_entity_catalog_rows_pg_returns_sparse_json_values() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-selected-catalog-{}", uuid::Uuid::new_v4());
        let entity_type = "DesignLanguage";
        let entity_id = "dl-sparse";
        let fields = serde_json::json!({
            "Id": entity_id,
            "Name": "Sparse Projection",
            "Status": "Published",
            "JsonField": { "kind": "nested", "score": 9 },
            "Large": "x".repeat(4096),
        });
        let state = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "status": "Published",
            "Title": "State title",
            "fields": fields,
            "events": [{ "type": "ShouldNotLoad" }],
            "sequence_nr": 7,
            "total_event_count": 7,
        });

        store
            .upsert_query_projection_with_state(
                &tenant,
                entity_type,
                entity_id,
                "Published",
                state.get("fields").unwrap(),
                &state,
                7,
            )
            .await
            .unwrap();

        let rows = store
            .load_selected_entity_catalog_rows_pg(
                &tenant,
                entity_type,
                &[entity_id.to_string(), "missing".to_string()],
                &[
                    "Id".to_string(),
                    "Title".to_string(),
                    "JsonField".to_string(),
                    "Status".to_string(),
                    "Missing".to_string(),
                ],
            )
            .await
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity_id, entity_id);
        assert_eq!(rows[0].status, "Published");
        assert_eq!(rows[0].sequence_nr, 7);
        assert!(rows[0].state.is_none());
        assert_eq!(rows[0].fields["Id"], entity_id);
        assert_eq!(rows[0].fields["Title"], "State title");
        assert_eq!(rows[0].fields["JsonField"]["kind"], "nested");
        assert_eq!(rows[0].fields["Status"], "Published");
        assert!(rows[0].fields.get("Large").is_none());
        assert!(rows[0].fields.get("Missing").is_none());

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
fn query_field_index_page_orders_and_limits_inside_postgres() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-query-page-{}", uuid::Uuid::new_v4());
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
                &[("Sequence".to_string(), true)],
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
                &[("Sequence".to_string(), true)],
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
                &[("Sequence".to_string(), true)],
                0,
                1,
                true,
            )
            .await
            .unwrap();

        assert_eq!(ids, vec![missing_sequence_id.to_string()]);
        assert_eq!(count, Some(4));

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
fn upsert_query_projection_diffs_field_index_rows() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-projection-diff-{}", uuid::Uuid::new_v4());
        let entity_type = "File";
        let entity_id = "file-diff";

        store
            .upsert_query_projection(
                &tenant,
                entity_type,
                entity_id,
                "Ready",
                &serde_json::json!({
                    "content_hash": "sha256:old",
                    "mime_type": "text/plain",
                    "owner": "alice",
                }),
                1,
            )
            .await
            .unwrap();

        let owner_xmin_before: String = crate::dbm::postgres_query_scalar!(
            "SELECT xmin::text FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND field_name = $4",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind("owner")
        .fetch_one(&pool)
        .await
        .unwrap();
        let hash_xmin_before: String = crate::dbm::postgres_query_scalar!(
            "SELECT xmin::text FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND field_name = $4",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind("content_hash")
        .fetch_one(&pool)
        .await
        .unwrap();

        store
            .upsert_query_projection(
                &tenant,
                entity_type,
                entity_id,
                "Ready",
                &serde_json::json!({
                    "content_hash": "sha256:new",
                    "owner": "alice",
                    "size_bytes": 128,
                }),
                2,
            )
            .await
            .unwrap();

        let owner_xmin_after: String = crate::dbm::postgres_query_scalar!(
            "SELECT xmin::text FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND field_name = $4",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind("owner")
        .fetch_one(&pool)
        .await
        .unwrap();
        let hash_xmin_after: String = crate::dbm::postgres_query_scalar!(
            "SELECT xmin::text FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND field_name = $4",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind("content_hash")
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            owner_xmin_before, owner_xmin_after,
            "unchanged field index rows should not be rewritten"
        );
        assert_ne!(
            hash_xmin_before, hash_xmin_after,
            "changed field index rows should be rewritten"
        );

        let field_rows: Vec<(String, Option<String>, Option<String>)> =
            crate::dbm::postgres_query_as!(
                "SELECT field_name, field_value, status FROM entity_field_index \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                 ORDER BY field_name",
            )
            .bind(&tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            field_rows,
            vec![
                (
                    "content_hash".to_string(),
                    Some("sha256:new".to_string()),
                    Some("Ready".to_string())
                ),
                (
                    "owner".to_string(),
                    Some("alice".to_string()),
                    Some("Ready".to_string())
                ),
                (
                    "size_bytes".to_string(),
                    Some("128".to_string()),
                    Some("Ready".to_string())
                ),
            ]
        );

        let catalog_row: (String, serde_json::Value, i64) = crate::dbm::postgres_query_as!(
            "SELECT status, fields, sequence_nr FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(catalog_row.0, "Ready");
        assert_eq!(catalog_row.1["content_hash"], "sha256:new");
        assert_eq!(catalog_row.1["owner"], "alice");
        assert_eq!(catalog_row.1["size_bytes"], 128);
        assert_eq!(catalog_row.2, 2);

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
fn native_data_only_create_inserts_event_catalog_and_index_atomically() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-native-data-only-{}", uuid::Uuid::new_v4());
        let entity_type = "SessionEntry";
        let entity_id = "entry-native";
        let fields = serde_json::json!({
            "Id": entity_id,
            "SessionId": "ss-native",
            "EntryId": entity_id,
            "Role": "assistant",
            "Sequence": 2,
            "Content": "hello"
        });
        let mut envelope = test_envelope("Created", fields.clone());
        envelope.sequence_nr = 1;

        let sequence_nr = store
            .create_data_only_entity_native(
                &tenant,
                entity_type,
                entity_id,
                "Active",
                &fields,
                &envelope,
            )
            .await
            .unwrap();

        assert_eq!(sequence_nr, 1);
        let event_count: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COUNT(*)::bigint FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(event_count, 1);

        let catalog: Option<(String, serde_json::Value, Option<serde_json::Value>, i64)> =
            crate::dbm::postgres_query_as!(
                "SELECT status, fields, state, sequence_nr FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(&tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
        let (status, stored_fields, stored_state, stored_sequence) = catalog.unwrap();
        assert_eq!(status, "Active");
        assert_eq!(stored_fields, fields);
        let stored_state = stored_state.expect("native create should store full catalog state");
        assert_eq!(stored_state["fields"], fields);
        assert_eq!(stored_state["sequence_nr"], 1);
        assert_eq!(stored_state["total_event_count"], 1);
        assert_eq!(stored_sequence, 1);

        let indexed_count: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COUNT(*)::bigint FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(indexed_count >= 5);

        let duplicate = store
            .create_data_only_entity_native(
                &tenant,
                entity_type,
                entity_id,
                "Active",
                &fields,
                &envelope,
            )
            .await;
        assert!(matches!(
            duplicate,
            Err(PersistenceError::ConcurrencyViolation { .. })
        ));

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM events WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
fn upsert_query_projection_advances_sequence_without_rewriting_unchanged_index() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-projection-noop-{}", uuid::Uuid::new_v4());
        let entity_type = "Session";
        let entity_id = "session-noop";
        let fields = serde_json::json!({
            "title": "Latency proof",
            "owner": "temper",
            "message_count": 7,
        });

        store
            .upsert_query_projection(&tenant, entity_type, entity_id, "Active", &fields, 1)
            .await
            .unwrap();

        let owner_xmin_before: String = crate::dbm::postgres_query_scalar!(
            "SELECT xmin::text FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND field_name = $4",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind("owner")
        .fetch_one(&pool)
        .await
        .unwrap();

        store
            .upsert_query_projection(&tenant, entity_type, entity_id, "Active", &fields, 2)
            .await
            .unwrap();

        let owner_xmin_after: String = crate::dbm::postgres_query_scalar!(
            "SELECT xmin::text FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND field_name = $4",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind("owner")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            owner_xmin_before, owner_xmin_after,
            "no-op projection updates should not rewrite unchanged field index rows"
        );

        let catalog_row: (String, serde_json::Value, i64) = crate::dbm::postgres_query_as!(
            "SELECT status, fields, sequence_nr FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(catalog_row.0, "Active");
        assert_eq!(catalog_row.1, fields);
        assert_eq!(catalog_row.2, 2);

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
fn upsert_query_projection_updates_index_status_when_fields_are_unchanged() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-projection-status-{}", uuid::Uuid::new_v4());
        let entity_type = "Session";
        let entity_id = "session-status";
        let fields = serde_json::json!({
            "title": "Latency proof",
            "owner": "temper",
            "message_count": 7,
        });

        store
            .upsert_query_projection(&tenant, entity_type, entity_id, "Active", &fields, 1)
            .await
            .unwrap();
        store
            .upsert_query_projection(&tenant, entity_type, entity_id, "Complete", &fields, 2)
            .await
            .unwrap();

        let field_rows: Vec<(String, Option<String>)> = crate::dbm::postgres_query_as!(
            "SELECT field_name, status FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
             ORDER BY field_name",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            field_rows,
            vec![
                ("message_count".to_string(), Some("Complete".to_string())),
                ("owner".to_string(), Some("Complete".to_string())),
                ("title".to_string(), Some("Complete".to_string())),
            ]
        );

        let catalog_row: (String, serde_json::Value, i64) = crate::dbm::postgres_query_as!(
            "SELECT status, fields, sequence_nr FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(catalog_row.0, "Complete");
        assert_eq!(catalog_row.1, fields);
        assert_eq!(catalog_row.2, 2);

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
fn upsert_query_projection_clears_index_when_scalar_fields_disappear() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-projection-empty-index-{}", uuid::Uuid::new_v4());
        let entity_type = "Session";
        let entity_id = "session-empty-index";

        store
            .upsert_query_projection(
                &tenant,
                entity_type,
                entity_id,
                "Active",
                &serde_json::json!({
                    "title": "Latency proof",
                    "owner": "temper",
                    "message_count": 7,
                }),
                1,
            )
            .await
            .unwrap();

        let indexed_before: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COUNT(*)::bigint FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(indexed_before, 3);

        let fields = serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}],
            "metadata": {"origin": "test"},
            "archived_reason": null,
        });
        store
            .upsert_query_projection(&tenant, entity_type, entity_id, "Active", &fields, 2)
            .await
            .unwrap();

        let indexed_after: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COUNT(*)::bigint FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(indexed_after, 0);

        let catalog_row: (String, serde_json::Value, i64) = crate::dbm::postgres_query_as!(
            "SELECT status, fields, sequence_nr FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(catalog_row.0, "Active");
        assert_eq!(catalog_row.1, fields);
        assert_eq!(catalog_row.2, 2);

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}

#[test]
fn upsert_query_projection_removes_index_row_when_value_becomes_too_long() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-projection-long-field-{}", uuid::Uuid::new_v4());
        let entity_type = "File";
        let entity_id = "file-long-field";

        store
            .upsert_query_projection(
                &tenant,
                entity_type,
                entity_id,
                "Ready",
                &serde_json::json!({"description": "short"}),
                1,
            )
            .await
            .unwrap();

        let indexed_before: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COUNT(*)::bigint FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND field_name = $4",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind("description")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(indexed_before, 1);

        let long_description = "x".repeat(2500);
        store
            .upsert_query_projection(
                &tenant,
                entity_type,
                entity_id,
                "Ready",
                &serde_json::json!({"description": long_description}),
                2,
            )
            .await
            .unwrap();

        let indexed_after: i64 = crate::dbm::postgres_query_scalar!(
            "SELECT COUNT(*)::bigint FROM entity_field_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND field_name = $4",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind("description")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(indexed_after, 0);

        let catalog_description: Option<String> = crate::dbm::postgres_query_scalar!(
            "SELECT fields ->> 'description' FROM entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let expected_description = "x".repeat(2500);
        assert_eq!(
            catalog_description.as_deref(),
            Some(expected_description.as_str())
        );

        crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
        crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .unwrap();
    });
}
