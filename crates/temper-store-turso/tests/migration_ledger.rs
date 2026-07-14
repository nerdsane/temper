use libsql::Builder;
use temper_store_turso::TursoEventStore;

#[tokio::test]
async fn incompatible_schema_object_prevents_startup() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database_path = directory.path().join("incompatible-schema.db");

    let database = Builder::new_local(&database_path)
        .build()
        .await
        .expect("create legacy database");
    let connection = database.connect().expect("connect to legacy database");
    connection
        .execute(
            "CREATE VIEW tenant_installed_apps AS \
             SELECT 'tenant' AS tenant_id, 'app' AS app_name",
            (),
        )
        .await
        .expect("install incompatible schema object");
    drop(connection);
    drop(database);

    let url = format!("file:{}", database_path.display());
    let error = TursoEventStore::new(&url, None)
        .await
        .expect_err("an incompatible schema object must prevent store readiness");

    let diagnostic = error.to_string();
    assert!(
        diagnostic.contains("tenant_installed_apps"),
        "diagnostic must name the incompatible capability: {diagnostic}"
    );
    assert!(
        diagnostic.contains("must be a table") && diagnostic.contains("found view"),
        "diagnostic must explain the incompatible object kind: {diagnostic}"
    );
    assert!(
        diagnostic.contains("migration 4") && diagnostic.contains("apps-platform-and-secrets"),
        "diagnostic must identify the incompatible migration: {diagnostic}"
    );
}
