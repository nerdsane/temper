//! PostgreSQL schema selection survives the standard server's metadata paths.
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{
    ServerState, registry::SpecRegistry, registry_bootstrap::restore_registry_from_postgres_store,
    secrets::vault::SecretsVault, storage::StorageStack,
};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn standard_server_keeps_schema_for_secrets_wasm_and_spec_restoration() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
    let options: PgConnectOptions = url.parse().unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(options.options([("search_path", "pg_catalog")]))
        .await
        .unwrap();
    let name = format!(
        "server_schema_{}",
        temper_runtime::scheduler::sim_uuid().simple()
    );
    let schema = PostgresSchema::new(name).unwrap();
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    let mut state =
        ServerState::from_registry(ActorSystem::new("schema-test"), SpecRegistry::new())
            .with_secrets_vault(SecretsVault::new(&[7u8; 32]));
    let data_dir = tempfile::tempdir().unwrap();
    state.data_dir = data_dir.path().to_path_buf();
    state.set_storage_stack(StorageStack::from_postgres(store.clone()));
    let vault = state.secrets_vault.as_ref().unwrap();
    let (ciphertext, nonce) = vault.encrypt(b"test-value").unwrap();
    state
        .write_secret(
            "tenant",
            "KEY",
            &ciphertext,
            &nonce,
            temper_runtime::persistence::SecretPrecondition::Any,
        )
        .await
        .unwrap();
    assert_eq!(state.load_tenant_secrets("tenant").await.unwrap(), 1);
    assert_eq!(vault.get_secret("tenant", "KEY").unwrap(), "test-value");
    state
        .upsert_wasm_module("tenant", "module", b"\0asm\x01\0\0\0", "", "upload")
        .await
        .unwrap();
    assert_eq!(
        state.load_wasm_module_sources("tenant").await.unwrap()["module"].source,
        "upload"
    );
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    state
        .upsert_spec_source("tenant", "Order", order_ioa, csdl_xml)
        .await
        .unwrap();
    state
        .upsert_tenant_constraints("tenant", None)
        .await
        .unwrap();
    // A spec is restored only after its install committed.
    let order = std::collections::BTreeMap::from([("Order".to_string(), order_ioa.to_string())]);
    assert_eq!(
        state
            .commit_spec_sources("tenant", &order, csdl_xml, &[])
            .await
            .unwrap(),
        Some(true)
    );
    let mut restored = SpecRegistry::new();
    assert_eq!(
        restore_registry_from_postgres_store(&mut restored, &store)
            .await
            .unwrap(),
        1
    );
    assert!(
        restored
            .get_spec(&TenantId::new("tenant"), "Order")
            .is_some()
    );
    assert!(state.delete_secret("tenant", "KEY").await.unwrap());
    let path: String = sqlx::query_scalar("SHOW search_path")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(path, "pg_catalog");
    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.qualify_sql("{schema}").trim_end_matches('.')
    ))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}
