use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::state::IndexedFileStreamRead;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

const FILE_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.FileReadFastPathTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="content_hash" Type="Edm.String"/>
        <Property Name="mime_type" Type="Edm.String"/>
        <Property Name="has_content" Type="Edm.Boolean"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Files" EntityType="Temper.FileReadFastPathTest.File"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const FILE_IOA: &str = r#"
[automaton]
name = "File"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "content_hash"
type = "string"
initial = ""

[[state]]
name = "mime_type"
type = "string"
initial = ""

[[state]]
name = "has_content"
type = "bool"
initial = "false"
"#;

async fn build_turso_state(test_name: &str) -> (ServerState, TursoEventStore) {
    let db_path = std::env::temp_dir().join(format!(
        "temper-file-value-fast-path-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let mut state = ServerState::from_registry(ActorSystem::new(test_name), SpecRegistry::new());
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store)
}

async fn build_turso_file_state(test_name: &str) -> (ServerState, TursoEventStore) {
    let db_path = std::env::temp_dir().join(format!(
        "temper-file-value-fast-path-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(FILE_CSDL_XML).expect("file CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        FILE_CSDL_XML.to_string(),
        &[("File", FILE_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(test_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store)
}

#[tokio::test]
async fn read_file_stream_indexed_returns_blob_without_actor_materialization() {
    let (state, store) = build_turso_state("content").await;
    let tenant = TenantId::default();
    let bytes = b"<main>published embodiment</main>";
    let content_hash = "sha256:fast-path-content";

    store
        .put_blob(&format!("temper-fs/{content_hash}"), bytes)
        .await
        .expect("put blob");
    store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "fl-fast-path",
            "Ready",
            &serde_json::json!({
                "content_hash": content_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert File projection");

    let read = state
        .read_file_stream_indexed(&tenant, "fl-fast-path")
        .await
        .expect("indexed file read succeeds");

    assert_eq!(
        read,
        IndexedFileStreamRead::Content {
            content_hash: content_hash.to_string(),
            mime_type: "text/html".to_string(),
            bytes: bytes.to_vec(),
        }
    );
}

#[tokio::test]
async fn read_file_stream_indexed_reports_missing_index_for_unprojected_file() {
    let (state, _store) = build_turso_state("missing-index").await;
    let tenant = TenantId::default();

    let read = state
        .read_file_stream_indexed(&tenant, "fl-missing")
        .await
        .expect("indexed file read succeeds");

    assert_eq!(read, IndexedFileStreamRead::MissingIndex);
}

#[tokio::test]
async fn read_file_stream_indexed_falls_back_to_file_state_when_projection_is_missing() {
    let (state, store) = build_turso_file_state("missing-projection-fallback").await;
    let tenant = TenantId::default();
    let bytes = b"<main>projection lag should not break publishing</main>";
    let content_hash = "sha256:file-state-fallback";

    store
        .put_blob(&format!("temper-fs/{content_hash}"), bytes)
        .await
        .expect("put blob");

    state
        .get_or_create_tenant_entity(
            &tenant,
            "File",
            "fl-state-only",
            serde_json::json!({
                "content_hash": content_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
        )
        .await
        .expect("create File state");
    store
        .remove_query_projection(tenant.as_str(), "File", "fl-state-only")
        .await
        .expect("remove File projection");

    let read = state
        .read_file_stream_indexed(&tenant, "fl-state-only")
        .await
        .expect("indexed file read should fall back to entity state");

    assert_eq!(
        read,
        IndexedFileStreamRead::Content {
            content_hash: content_hash.to_string(),
            mime_type: "text/html".to_string(),
            bytes: bytes.to_vec(),
        }
    );
}

#[tokio::test]
async fn read_file_stream_indexed_falls_back_to_file_state_when_projection_is_stale() {
    let (state, store) = build_turso_file_state("stale-projection-fallback").await;
    let tenant = TenantId::default();
    let current_bytes = b"<main>current file state should win</main>";
    let current_hash = "sha256:current-file-state";
    let stale_hash = "sha256:stale-projection";

    store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "fl-stale-state",
            "Ready",
            &serde_json::json!({
                "content_hash": stale_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert stale File projection");
    store
        .put_blob(&format!("temper-fs/{current_hash}"), current_bytes)
        .await
        .expect("put current blob");

    state
        .get_or_create_tenant_entity(
            &tenant,
            "File",
            "fl-stale-state",
            serde_json::json!({
                "content_hash": current_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
        )
        .await
        .expect("create File state");

    store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "fl-stale-state",
            "Ready",
            &serde_json::json!({
                "content_hash": stale_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("restore stale File projection after state write");

    let read = state
        .read_file_stream_indexed(&tenant, "fl-stale-state")
        .await
        .expect("indexed file read should fall back to entity state");

    assert_eq!(
        read,
        IndexedFileStreamRead::Content {
            content_hash: current_hash.to_string(),
            mime_type: "text/html".to_string(),
            bytes: current_bytes.to_vec(),
        }
    );
}

#[tokio::test]
async fn read_file_stream_indexed_reports_stale_index_when_blob_is_missing() {
    let (state, store) = build_turso_state("stale-index").await;
    let tenant = TenantId::default();
    let content_hash = "sha256:missing-blob";

    store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "fl-stale",
            "Ready",
            &serde_json::json!({
                "content_hash": content_hash,
                "mime_type": "text/html",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert File projection");

    let read = state
        .read_file_stream_indexed(&tenant, "fl-stale")
        .await
        .expect("indexed file read succeeds");

    assert_eq!(
        read,
        IndexedFileStreamRead::StaleIndex {
            content_hash: content_hash.to_string(),
            mime_type: "text/html".to_string(),
        }
    );
}
