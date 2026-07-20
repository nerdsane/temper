//! Postgres projection and published-artifact regressions.

use super::*;

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
