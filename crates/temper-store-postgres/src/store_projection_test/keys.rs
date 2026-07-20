//! Focused Postgres projection regression group.

use super::*;

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
                &[test_envelope(
                    "Delete",
                    serde_json::json!({
                        "action": "Delete",
                        "from_status": "Published",
                        "to_status": "Deleted"
                    }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &format!("{tenant}:{entity_type}:dl-action-named-live"),
                0,
                &[test_envelope(
                    "Transitioned",
                    serde_json::json!({
                        "action": "Deleted",
                        "from_status": "Draft",
                        "to_status": "Published"
                    }),
                )],
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
                "dl-action-named-live".to_string(),
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
