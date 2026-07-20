//! Focused File value-path regression group.

use super::*;

#[tokio::test]
async fn put_file_stream_content_writes_native_blob_and_dispatches_update() {
    let (mut state, _store) = build_turso_file_state("native-write").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    state
        .get_or_create_tenant_entity(&tenant, "File", "fl-native-write", serde_json::json!({}))
        .await
        .expect("create File state");

    let body = b"native File value write";
    let expected_hash = format!("sha256:{:x}", Sha256::digest(body));
    let response = state
        .put_file_stream_content(
            &tenant,
            "fl-native-write",
            body,
            "text/plain",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("native File content write should succeed");

    assert_eq!(response.state.status, "Ready");
    assert_eq!(response.state.fields["content_hash"], expected_hash);
    assert_eq!(response.state.fields["mime_type"], "text/plain");
    assert_eq!(response.state.fields["has_content"], true);
    assert_eq!(response.state.fields["size_bytes"], body.len() as i64);
    assert_local_blob(data_dir.path(), &expected_hash, body).await;
}

#[tokio::test]
async fn odata_file_value_put_uses_native_path_without_blob_adapter() {
    let (mut state, _store) = build_turso_file_state("odata-native-write").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    state
        .get_or_create_tenant_entity(&tenant, "File", "fl-odata-native", serde_json::json!({}))
        .await
        .expect("create File state");

    let app = build_router(state.clone());
    let body = b"odata native File value write";
    let expected_hash = format!("sha256:{:x}", Sha256::digest(body));
    let response = app
        .oneshot(
            Request::put("/tdata/Files('fl-odata-native')/$value")
                .header("content-type", "text/plain")
                .body(Body::from(body.as_slice()))
                .expect("request"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let expected_etag = format!("\"{expected_hash}\"");
    assert_eq!(
        response.headers().get("ETag").and_then(|v| v.to_str().ok()),
        Some(expected_etag.as_str())
    );

    let entity = state
        .get_tenant_entity_state(&tenant, "File", "fl-odata-native")
        .await
        .expect("OData native write should update File state");
    assert_eq!(entity.state.fields["content_hash"], expected_hash);
    assert_eq!(entity.state.fields["mime_type"], "text/plain");
    assert_eq!(entity.state.fields["has_content"], true);
    assert_local_blob(data_dir.path(), &expected_hash, body).await;
}

#[tokio::test]
async fn odata_file_value_put_applies_cedar_update_policy() {
    let (mut state, _store) = build_turso_file_state("odata-write-denied").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();
    mark_file_verified(&state);

    state
        .get_or_create_tenant_entity(&tenant, "File", "fl-write-denied", serde_json::json!({}))
        .await
        .expect("create File state");
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"permit(principal, action == Action::"read", resource is File);"#,
        )
        .expect("install Cedar policy");

    let response = build_router(state.clone())
        .oneshot(
            Request::put("/tdata/Files('fl-write-denied')/$value")
                .header("content-type", "text/plain")
                .header("x-temper-principal-kind", "customer")
                .header("x-temper-principal-id", "customer-1")
                .body(Body::from("must not be written"))
                .expect("request"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let entity = state
        .get_tenant_entity_state(&tenant, "File", "fl-write-denied")
        .await
        .expect("File state should remain readable");
    assert!(entity.state.fields.get("content_hash").is_none());
}

#[tokio::test]
async fn odata_file_value_get_applies_cedar_read_policy() {
    let (mut state, _store) = build_turso_file_state("odata-read-denied").await;
    let tenant = TenantId::default();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    state.data_dir = data_dir.path().to_path_buf();

    state
        .get_or_create_tenant_entity(&tenant, "File", "fl-read-denied", serde_json::json!({}))
        .await
        .expect("create File state");
    state
        .put_file_stream_content(
            &tenant,
            "fl-read-denied",
            b"private content",
            "text/plain",
            &AgentContext::for_service("test-writer"),
        )
        .await
        .expect("seed stream content");
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"permit(principal, action == Action::"update", resource is File);"#,
        )
        .expect("install Cedar policy");

    let response = build_router(state)
        .oneshot(
            Request::get("/tdata/Files('fl-read-denied')/$value")
                .header("x-temper-principal-kind", "customer")
                .header("x-temper-principal-id", "customer-1")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
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
