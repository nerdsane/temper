//! Focused Postgres projection regression group.

use super::*;

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
