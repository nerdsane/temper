use super::*;
use crate::migration::run_migrations;
use sqlx::{PgPool, postgres::PgPoolOptions};
use temper_runtime::persistence::{
    EntityKeyRow, EntityVectorRow, EventStore, IndexReconciliation, PersistenceAppend,
};

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

        // Exact reconciliation with an empty current key set purges A's stale
        // row in the same transaction as the replacement event.
        store
            .append_with_keys(
                &pid_a,
                1,
                &[test_envelope("Replace", serde_json::json!({}))],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Doc", "path", &key.key_hash)
                .await
                .unwrap(),
            None,
            "empty exact key set must purge the prior key row",
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

/// ADR-0153 backfill robustness: the real postgres store honors the exact,
/// watermark-gated reconciliation primitives — `backfill_entity_keys` (including an
/// empty-set purge), `keyed_entity_ids_for_type` (projection enumeration), and the
/// watermark round-trip
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

        // The real backend can acquire and release the distributed per-type
        // reconciliation fence before issuing its exact repair queries.
        let fence = store
            .acquire_projection_reconciliation_fence(&tenant, "Directory")
            .await
            .expect("acquire postgres projection fence");
        drop(fence);

        // Backfill (no journal event) keys a pre-existing entity — the root case.
        store
            .backfill_entity_keys(&tenant, "Directory", "dir-root", std::slice::from_ref(&key))
            .await
            .unwrap();

        // Projection enumeration: the entity now shows as keyed for the type.
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

        let conflict = store
            .backfill_entity_keys(
                &tenant,
                "Directory",
                "dir-conflict",
                std::slice::from_ref(&key),
            )
            .await
            .expect_err("duplicate live key must fail exact reconciliation");
        assert!(conflict.to_string().contains("duplicate declared key"));
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Directory", "name_parent", &key.key_hash)
                .await
                .unwrap(),
            Some("dir-root".to_string()),
            "failed exact repair must not disturb the existing holder"
        );

        // Empty exact reconciliation purges historical rows for a tombstone or
        // projection-only phantom, without appending a journal event.
        store
            .backfill_entity_keys(&tenant, "Directory", "dir-root", &[])
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Directory", "name_parent", &key.key_hash)
                .await
                .unwrap(),
            None,
        );

        // Once the historical holder is purged, a live append can atomically
        // reclaim that key. This is the production-backend half of the legacy
        // tombstone upgrade proof.
        store
            .append_with_keys(
                &format!("{tenant}:Directory:dir-replacement"),
                0,
                &[test_envelope("Create", serde_json::json!({}))],
                std::slice::from_ref(&key),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Directory", "name_parent", &key.key_hash)
                .await
                .unwrap(),
            Some("dir-replacement".to_string()),
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
fn projection_fence_waiters_do_not_starve_the_main_postgres_pool() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-fence-pool-{}", uuid::Uuid::new_v4());
        let fence = store
            .acquire_projection_reconciliation_fence(&tenant, "Directory")
            .await
            .expect("acquire exclusive projection fence");

        let writer_store = store.clone();
        let writer_tenant = tenant.clone();
        let writer = tokio::spawn(async move {
            let appends = ["Directory", "IndexedB", "IndexedC", "IndexedD", "IndexedE"]
                .into_iter()
                .map(|entity_type| PersistenceAppend {
                    persistence_id: format!("{writer_tenant}:{entity_type}:live"),
                    expected_sequence: 0,
                    events: vec![test_envelope("Create", serde_json::json!({}))],
                    key_rows: vec![EntityKeyRow {
                        key_name: "path".to_string(),
                        key_hash: format!("live-{entity_type}"),
                    }],
                    vector_rows: vec![EntityVectorRow {
                        decl_name: "embedding".to_string(),
                        model_tag: "m1".to_string(),
                        vector: vec![1.0, 0.0],
                    }],
                    reconciliation: IndexReconciliation {
                        keys: true,
                        vectors: true,
                    },
                })
                .collect::<Vec<_>>();
            writer_store.append_batch(&appends).await
        });
        // Wait until the writer owns a second dedicated lock-pool connection, then
        // yield so its shared advisory-lock query reaches PostgreSQL. It must remain
        // pending there, leaving the sole main-pool connection available to the
        // exclusive holder's repair query.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while store.projection_lock_pool.size() < 2 {
                tokio::task::yield_now().await;
            }
            tokio::task::yield_now().await;
        })
        .await
        .expect("writer did not acquire its dedicated lock-pool connection");
        assert!(
            !writer.is_finished(),
            "live writer unexpectedly bypassed the exclusive projection fence"
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            store.backfill_entity_keys(
                &tenant,
                "Directory",
                "repair",
                &[EntityKeyRow {
                    key_name: "path".to_string(),
                    key_hash: "repair-path".to_string(),
                }],
            ),
        )
        .await
        .expect("exclusive holder was starved by a lock waiter")
        .expect("repair under exclusive fence");

        drop(fence);
        let results = tokio::time::timeout(std::time::Duration::from_secs(5), writer)
            .await
            .expect("five-partition batch self-deadlocked in the lock pool")
            .expect("writer task joined")
            .expect("writer committed after fence release");
        assert_eq!(results.len(), 5);
        assert_eq!(
            store
                .lookup_by_key(&tenant, "Directory", "path", "live-Directory")
                .await
                .expect("lookup batch key")
                .as_deref(),
            Some("live")
        );
        let vectors = store
            .vector_candidates(&tenant, "IndexedE", "embedding", "m1", 10)
            .await
            .expect("read batch vectors");
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].entity_id, "live");

        let _ = crate::dbm::postgres_query!("DELETE FROM entity_key_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await;
        let _ = crate::dbm::postgres_query!("DELETE FROM entity_vector_index WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await;
        let _ = crate::dbm::postgres_query!("DELETE FROM events WHERE tenant = $1")
            .bind(&tenant)
            .execute(&pool)
            .await;
    });
}

#[test]
fn maintenance_import_invalidates_projection_authority_atomically() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-import-authority-{}", uuid::Uuid::new_v4());

        store
            .mark_key_index_backfilled(&tenant, "Document", "v2|keys")
            .await
            .unwrap();
        store
            .mark_vector_index_backfilled(&tenant, "Document", "v2|vectors")
            .await
            .unwrap();

        store
            .invalidate_projection_backfill_watermarks(&tenant)
            .await
            .unwrap();

        assert!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .vector_index_backfilled_types(&tenant)
                .await
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn list_entity_ids_by_type_unions_catalog_field_index_and_events() {
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

        assert_eq!(
            ids,
            vec![
                "dl-catalog".to_string(),
                "dl-event".to_string(),
                "dl-index".to_string(),
            ]
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
