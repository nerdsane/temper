use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::secrets::vault::SecretsVault;
use temper_server::state::PublishFileArtifactRequest;
use temper_server::storage::StorageStack;
use temper_store_turso::TursoEventStore;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TENANT: &str = "default";
const CONTENT_HASH: &str = "sha256:published-artifact-content";
const FILE_ID: &str = "file-published-artifact-test";

fn secrets_vault(blob_endpoint: &str) -> SecretsVault {
    let vault = SecretsVault::new(&[9u8; 32]);
    vault
        .cache_secret(
            TENANT,
            "published_blob_public_base_url",
            format!("{blob_endpoint}/public"),
        )
        .expect("cache public base URL");
    vault
        .cache_secret(TENANT, "published_blob_endpoint", blob_endpoint.to_string())
        .expect("cache public blob endpoint");
    vault
        .cache_secret(
            TENANT,
            "published_blob_bucket",
            "published-bucket".to_string(),
        )
        .expect("cache public blob bucket");
    vault
}

fn publish_request() -> PublishFileArtifactRequest {
    PublishFileArtifactRequest {
        file_id: FILE_ID.to_string(),
        label: "latest".to_string(),
        owner_ref_type: "Doc".to_string(),
        owner_ref_id: "doc-a".to_string(),
        source_file_version_id: String::new(),
        namespace: Some("publish-test".to_string()),
    }
}

async fn mock_public_blob_endpoint() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn publish_file_artifact_persists_metadata_through_turso_metadata_store() {
    let mock_blob = mock_public_blob_endpoint().await;
    let db_path = std::env::temp_dir().join(format!(
        "temper-published-artifacts-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let mut state = ServerState::from_registry(
        ActorSystem::new("published-artifacts-turso-test"),
        SpecRegistry::new(),
    )
    .with_secrets_vault(secrets_vault(&mock_blob.uri()));
    state.set_storage_stack(StorageStack::from_turso(store.clone()));

    store
        .put_blob(
            &format!("temper-fs/{CONTENT_HASH}"),
            b"public artifact body",
        )
        .await
        .expect("put source blob");
    store
        .upsert_query_projection(
            TENANT,
            "File",
            FILE_ID,
            "Ready",
            &serde_json::json!({
                "content_hash": CONTENT_HASH,
                "mime_type": "text/markdown",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert File projection");

    let artifact = state
        .publish_file_artifact(&TenantId::default(), publish_request())
        .await
        .expect("publish should succeed");

    let loaded = store
        .load_published_artifact(TENANT, &artifact.id)
        .await
        .expect("load published artifact")
        .expect("published artifact should be persisted");
    assert_eq!(loaded.public_url, artifact.public_url);
    assert_eq!(loaded.public_storage_key, artifact.public_storage_key);
    assert_eq!(loaded.owner_ref_type, "Doc");
    mock_blob.verify().await;
}

#[tokio::test]
async fn publish_file_artifact_persists_metadata_through_postgres_metadata_store_when_available() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };
    let mock_blob = mock_public_blob_endpoint().await;
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect postgres");
    temper_store_postgres::migration::run_migrations(&pool)
        .await
        .expect("run postgres migrations");
    let store = temper_store_postgres::PostgresEventStore::new(pool.clone());
    let tenant = format!("published-artifacts-{}", uuid::Uuid::new_v4());

    let vault = SecretsVault::new(&[10u8; 32]);
    vault
        .cache_secret(
            &tenant,
            "published_blob_public_base_url",
            format!("{}/public", mock_blob.uri()),
        )
        .expect("cache public base URL");
    vault
        .cache_secret(&tenant, "published_blob_endpoint", mock_blob.uri())
        .expect("cache public blob endpoint");
    vault
        .cache_secret(
            &tenant,
            "published_blob_bucket",
            "published-bucket".to_string(),
        )
        .expect("cache public blob bucket");

    let mut state = ServerState::from_registry(
        ActorSystem::new("published-artifacts-postgres-test"),
        SpecRegistry::new(),
    )
    .with_secrets_vault(vault);
    state.set_storage_stack(StorageStack::from_postgres(store.clone()));

    store
        .put_blob(
            &format!("temper-fs/{CONTENT_HASH}"),
            b"public artifact body",
        )
        .await
        .expect("put source blob");
    store
        .upsert_query_projection(
            &tenant,
            "File",
            FILE_ID,
            "Ready",
            &serde_json::json!({
                "content_hash": CONTENT_HASH,
                "mime_type": "text/markdown",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert File projection");

    let artifact = state
        .publish_file_artifact(&TenantId::new(&tenant), publish_request())
        .await
        .expect("publish should succeed");

    let loaded = store
        .load_published_artifact(&tenant, &artifact.id)
        .await
        .expect("load published artifact")
        .expect("published artifact should be persisted");
    assert_eq!(loaded.public_url, artifact.public_url);
    assert_eq!(loaded.public_storage_key, artifact.public_storage_key);
    assert_eq!(loaded.owner_ref_id, "doc-a");

    sqlx::query("DELETE FROM published_artifacts WHERE tenant = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("cleanup published_artifacts");
    sqlx::query("DELETE FROM entity_field_index WHERE tenant = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("cleanup entity_field_index");
    sqlx::query("DELETE FROM entity_catalog WHERE tenant = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("cleanup entity_catalog");
    mock_blob.verify().await;
}
