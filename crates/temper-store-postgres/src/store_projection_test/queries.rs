//! Focused Postgres projection regression group.

use super::*;

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
