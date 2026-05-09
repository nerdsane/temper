use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::state::IndexedFileStreamRead;
use temper_server::{ServerEventStore, ServerState};
use temper_store_turso::TursoEventStore;

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
    state.event_store = Some(Arc::new(ServerEventStore::Turso(store.clone())));
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
