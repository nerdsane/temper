//! Focused File value-path regression group.

use super::*;

async fn build_turso_file_workspace_state(test_name: &str) -> (ServerState, TursoEventStore) {
    let db_path = std::env::temp_dir().join(format!(
        "temper-file-value-fast-path-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(FILE_WORKSPACE_CSDL_XML).expect("file+workspace CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        FILE_WORKSPACE_CSDL_XML.to_string(),
        &[("File", FILE_IOA_GUARDED), ("Workspace", WORKSPACE_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(test_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store)
}

/// Create a Workspace and Freeze it so `resolve_entity_status` reports
/// `"Frozen"`.
async fn freeze_workspace(state: &ServerState, tenant: &TenantId, workspace_id: &str) {
    state
        .get_or_create_tenant_entity(tenant, "Workspace", workspace_id, serde_json::json!({}))
        .await
        .expect("create Workspace state");
    state
        .dispatch_tenant_action(
            tenant,
            "Workspace",
            workspace_id,
            "Freeze",
            serde_json::json!({}),
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("freeze Workspace");
}

#[tokio::test]
async fn create_file_in_frozen_workspace_is_rejected_with_no_blob() {
    let (mut state, _store) = build_turso_file_workspace_state("create-frozen-ws").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    freeze_workspace(&state, &tenant, "ws-frozen").await;

    let body = b"content for a frozen workspace";
    let content_hash = format!("sha256:{:x}", Sha256::digest(body));
    let result = state
        .create_file_with_initial_stream_content(
            &tenant,
            "fl-in-frozen",
            serde_json::json!({
                "name": "blocked.md",
                "path": "/blocked.md",
                "directory_id": "dir-root",
                "workspace_id": "ws-frozen",
                "mime_type": "text/markdown",
            }),
            body,
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await;

    let error = result.expect_err("write into a Frozen workspace must be rejected");
    assert!(
        error.contains("ws-frozen") && error.contains("Frozen"),
        "rejection must name the frozen workspace, got: {error}"
    );

    // No bytes were written: the pre-write check fires before the blob write.
    let blob_path = data_dir
        .path()
        .join("blobs")
        .join("temper-fs")
        .join(&content_hash);
    assert!(
        tokio::fs::metadata(&blob_path).await.is_err(),
        "no blob should be persisted when the workspace rejects the write"
    );

    // No File entity should exist either.
    assert!(
        !state
            .ensure_entity_loaded(&tenant, "File", "fl-in-frozen")
            .await,
        "File must not be created when its workspace is Frozen"
    );
}

#[tokio::test]
async fn put_existing_file_in_frozen_workspace_is_rejected_with_no_new_blob() {
    let (mut state, _store) = build_turso_file_workspace_state("put-frozen-ws").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    // Create the File while the workspace is still Active (so the first write
    // succeeds and the File persists its workspace_id), then freeze.
    state
        .create_file_with_initial_stream_content(
            &tenant,
            "fl-existing",
            serde_json::json!({
                "name": "doc.md",
                "path": "/doc.md",
                "directory_id": "dir-root",
                "workspace_id": "ws-later-frozen",
                "mime_type": "text/markdown",
            }),
            b"first version while active",
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("first write into an Active workspace should succeed");

    freeze_workspace(&state, &tenant, "ws-later-frozen").await;

    let body = b"second version after freeze";
    let content_hash = format!("sha256:{:x}", Sha256::digest(body));
    let result = state
        .put_file_stream_content(
            &tenant,
            "fl-existing",
            body,
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await;

    let error = result.expect_err("updating a File in a Frozen workspace must be rejected");
    assert!(
        error.contains("ws-later-frozen") && error.contains("Frozen"),
        "rejection must name the frozen workspace, got: {error}"
    );

    let blob_path = data_dir
        .path()
        .join("blobs")
        .join("temper-fs")
        .join(&content_hash);
    assert!(
        tokio::fs::metadata(&blob_path).await.is_err(),
        "the second (rejected) write must not persist a new blob"
    );
}

#[tokio::test]
async fn create_file_in_active_workspace_succeeds() {
    let (mut state, _store) = build_turso_file_workspace_state("create-active-ws").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    // Active workspace present — write must pass the gate AND the cross-entity
    // guard on StreamUpdated (which the synthetic path resolves to true).
    state
        .get_or_create_tenant_entity(&tenant, "Workspace", "ws-active", serde_json::json!({}))
        .await
        .expect("create Workspace state");

    let body = b"content for an active workspace";
    let response = state
        .create_file_with_initial_stream_content(
            &tenant,
            "fl-in-active",
            serde_json::json!({
                "name": "ok.md",
                "path": "/ok.md",
                "directory_id": "dir-root",
                "workspace_id": "ws-active",
                "mime_type": "text/markdown",
            }),
            body,
            "text/markdown",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("write into an Active workspace should succeed");

    assert_eq!(response.state.status, "Ready");
    assert_eq!(response.state.fields["has_content"], true);
}
