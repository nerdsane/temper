//! Real PostgreSQL regression for application-selected schemas.
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn store_uses_selected_schema_without_connection_search_path() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL URL");
    let options: PgConnectOptions = url.parse().unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(options.clone().options([("search_path", "pg_catalog")]))
        .await
        .unwrap();
    let schema = format!("temper_schema_{}", uuid::Uuid::new_v4().simple());
    let selected = PostgresSchema::new(&schema).unwrap();
    let (left, right) = tokio::join!(
        migration::run_migrations_in_schema(&pool, &selected),
        migration::run_migrations_in_schema(&pool, &selected),
    );
    left.unwrap();
    right.unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), selected);
    let result = store
        .upsert_spec("tenant", "Counter", "source", "csdl", "hash")
        .await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    result.expect("store must use the selected schema even when search_path excludes it");
    pool.close().await;
}

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};

fn envelope(value: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Created".into(),
        payload: serde_json::json!({"value":value}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "schema-test".into(),
        },
    }
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn schemas_share_a_pool_without_sharing_data_or_changing_defaults() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").unwrap();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let legacy = PostgresSchema::new(format!("legacy_{suffix}")).unwrap();
    let a = PostgresSchema::new(format!("A.\"_{suffix}")).unwrap();
    let b = PostgresSchema::new(format!("b_{suffix}")).unwrap();
    let options: PgConnectOptions = url.parse().unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.options([("search_path", legacy.name().unwrap())]))
        .await
        .unwrap();
    for schema in [&legacy, &a, &b] {
        migration::run_migrations_in_schema(&pool, schema)
            .await
            .unwrap();
        migration::run_migrations_in_schema(&pool, schema)
            .await
            .unwrap();
        let applied: i64 = sqlx::query_scalar(
            &schema.qualify_sql("SELECT count(*) FROM {schema}_sqlx_migrations"),
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(applied >= 16);
    }
    // Existing constructor and migration entry point still use the old default.
    migration::run_migrations(&pool).await.unwrap();
    let old = PostgresEventStore::new(pool.clone());
    let first = PostgresEventStore::with_schema(pool.clone(), a.clone());
    let second = PostgresEventStore::with_schema(pool.clone(), b.clone());
    let pid = "tenant:Counter:one";
    for (store, value) in [(&old, "old"), (&first, "first"), (&second, "second")] {
        store.append(pid, 0, &[envelope(value)]).await.unwrap();
        store.save_snapshot(pid, 1, value.as_bytes()).await.unwrap();
        store.append(pid, 1, &[envelope(value)]).await.unwrap();
        store
            .upsert_spec("tenant", "Counter", value, "csdl", value)
            .await
            .unwrap();
        store.commit_specs("tenant").await.unwrap();
        store.put_blob("same-key", value.as_bytes()).await.unwrap();
        store
            .write_secret(&temper_runtime::persistence::SecretWrite {
                tenant: "tenant",
                key_name: "same-key",
                ciphertext: value.as_bytes(),
                nonce: b"nonce",
                version_id: value,
                precondition: temper_runtime::persistence::SecretPrecondition::Any,
            })
            .await
            .unwrap();
        store
            .save_policy("tenant", "policy", value, "tester")
            .await
            .unwrap();
        store
            .upsert_wasm_module("tenant", "module", value.as_bytes(), value, "upload")
            .await
            .unwrap();
        store
            .upsert_query_projection(
                "tenant",
                "Counter",
                "one",
                "Ready",
                &serde_json::json!({"color":value}),
                2,
            )
            .await
            .unwrap();
    }
    let clause = "entity_id IN (SELECT entity_id FROM entity_field_index WHERE tenant=?1 AND entity_type=?2 AND field_name=?3 AND field_value=?4)";
    for (store, value) in [(&old, "old"), (&first, "first"), (&second, "second")] {
        assert_eq!(store.read_events(pid, 0).await.unwrap().len(), 2);
        assert_eq!(
            store.read_events(pid, 0).await.unwrap()[0].payload["value"],
            value
        );
        assert_eq!(
            store.load_snapshot(pid).await.unwrap().unwrap().1,
            value.as_bytes()
        );
        assert_eq!(store.load_specs().await.unwrap()[0].ioa_source, value);
        assert_eq!(
            store.get_blob("same-key").await.unwrap().unwrap(),
            value.as_bytes()
        );
        assert_eq!(
            store.load_secrets_for_tenant("tenant").await.unwrap()[0].1,
            value.as_bytes()
        );
        assert_eq!(
            store.load_policies_for_tenant("tenant").await.unwrap()[0].cedar_text,
            value
        );
        assert_eq!(
            store
                .load_wasm_module("tenant", "module")
                .await
                .unwrap()
                .unwrap()
                .wasm_bytes,
            value.as_bytes()
        );
        assert_eq!(
            store
                .query_field_index(
                    "tenant",
                    "Counter",
                    clause,
                    vec!["color".into(), value.into()]
                )
                .await
                .unwrap(),
            vec!["one"]
        );
        let page = store
            .query_field_index_page(
                "tenant",
                "Counter",
                clause,
                vec!["color".into(), value.into()],
                &[],
                0,
                10,
                true,
            )
            .await
            .unwrap();
        assert_eq!(page, (vec!["one".into()], Some(1)));
        let past_end = store
            .query_field_index_page(
                "tenant",
                "Counter",
                clause,
                vec!["color".into(), value.into()],
                &[],
                10,
                10,
                true,
            )
            .await
            .unwrap();
        assert_eq!(past_end, (vec![], Some(1)));
    }
    assert!(
        first
            .query_field_index(
                "tenant",
                "Counter",
                clause,
                vec!["color".into(), "second".into()]
            )
            .await
            .unwrap()
            .is_empty()
    );
    first.delete_secret("tenant", "same-key").await.unwrap();
    first
        .remove_query_projection("tenant", "Counter", "one")
        .await
        .unwrap();
    assert_eq!(
        second
            .load_secrets_for_tenant("tenant")
            .await
            .unwrap()
            .len(),
        1
    );
    let unchanged: String = sqlx::query_scalar("SHOW search_path")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(unchanged, legacy.name().unwrap());
    for schema in [&a, &b, &legacy] {
        sqlx::query(&format!(
            "DROP SCHEMA {} CASCADE",
            schema.qualify_sql("{schema}").trim_end_matches('.')
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;
}
