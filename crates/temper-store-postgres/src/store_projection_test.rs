use super::*;
use crate::migration::run_migrations;
use sqlx::PgPool;

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
