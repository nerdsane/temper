use super::*;

#[tokio::test]
async fn published_artifact_upsert_round_trips_and_updates_by_id() {
    let store = make_store("published-artifacts").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    let artifact = PublishedArtifactUpsert {
        id: "part-test".to_string(),
        tenant: tenant.clone(),
        source_file_id: "fl-source".to_string(),
        source_file_version_id: "fv-source-v1".to_string(),
        content_hash: "sha256:first".to_string(),
        label: "preview".to_string(),
        mime_type: "image/png".to_string(),
        byte_length: 42,
        public_storage_key: "demo/documents/doc-1/preview-sha256:first.png".to_string(),
        public_url: "https://artifacts.example.com/demo/documents/doc-1/preview-sha256:first.png"
            .to_string(),
        owner_ref_type: "Document".to_string(),
        owner_ref_id: "doc-1".to_string(),
        status: "published".to_string(),
    };

    let inserted = store
        .upsert_published_artifact(&artifact)
        .await
        .expect("insert published artifact");
    assert_eq!(inserted.id, artifact.id);
    assert_eq!(inserted.public_url, artifact.public_url);

    let mut updated = artifact;
    updated.source_file_version_id = "fv-source-v2".to_string();
    updated.content_hash = "sha256:second".to_string();
    updated.byte_length = 84;
    updated.public_storage_key = "demo/documents/doc-1/preview-sha256:second.png".to_string();
    updated.public_url =
        "https://artifacts.example.com/demo/documents/doc-1/preview-sha256:second.png".to_string();

    store
        .upsert_published_artifact(&updated)
        .await
        .expect("update published artifact");
    let loaded = store
        .load_published_artifact(&tenant, "part-test")
        .await
        .expect("load published artifact")
        .expect("published artifact exists");

    assert_eq!(loaded.source_file_version_id, "fv-source-v2");
    assert_eq!(loaded.content_hash, "sha256:second");
    assert_eq!(loaded.byte_length, 84);
    assert_eq!(loaded.public_url, updated.public_url);
}

#[tokio::test]
async fn export_query_projections_returns_all_fields_for_migration() {
    let store = make_store("query-projection-export").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    store
        .upsert_query_projection(
            &tenant,
            "File",
            "file-a",
            "Ready",
            &serde_json::json!({
                "content_hash": "sha256:file-a",
                "has_content": true,
                "size_bytes": 12,
            }),
            9,
        )
        .await
        .expect("upsert projection");

    let rows = store
        .export_query_projections(Some(&tenant))
        .await
        .expect("export query projections");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tenant, tenant);
    assert_eq!(rows[0].entity_type, "File");
    assert_eq!(rows[0].entity_id, "file-a");
    assert_eq!(rows[0].status, "Ready");
    assert_eq!(rows[0].sequence_nr, 9);
    assert_eq!(
        rows[0].fields.get("content_hash").and_then(|v| v.as_str()),
        Some("sha256:file-a")
    );
    assert_eq!(
        rows[0].fields.get("has_content").and_then(|v| v.as_str()),
        None
    );
    assert_eq!(rows[0].fields["has_content"], true);
    assert_eq!(
        rows[0].fields.get("size_bytes").and_then(|v| v.as_str()),
        None
    );
    assert_eq!(rows[0].fields["size_bytes"], 12);
}

#[tokio::test]
async fn list_blobs_returns_rows_for_migration() {
    let store = make_store("blob-list").await;

    store
        .put_blob("temper-fs/sha256:abc", b"hello")
        .await
        .expect("put blob");

    let rows = store.list_blobs(100).await.expect("list blobs");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].blob_key, "temper-fs/sha256:abc");
    assert_eq!(rows[0].data, b"hello");
    assert_eq!(rows[0].size_bytes, 5);
    assert!(!rows[0].created_at.is_empty());
    assert_eq!(rows[0].expires_at, None);
}

#[tokio::test]
async fn load_wasm_module_metadata_all_tenants_returns_metadata_without_bulk_bytes() {
    let store = make_store("wasm-metadata").await;

    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a", "bundled")
        .await
        .expect("persist mod-a");
    store
        .upsert_wasm_module("tenant-b", "mod-b", b"hello-b", "hash-b", "bundled")
        .await
        .expect("persist mod-b");

    let rows = store
        .load_wasm_module_metadata_all_tenants()
        .await
        .expect("load wasm metadata");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].tenant, "tenant-a");
    assert_eq!(rows[0].module_name, "mod-a");
    assert_eq!(rows[0].sha256_hash, "hash-a");
    assert_eq!(rows[0].size_bytes, 7);
    assert!(!rows[0].updated_at.is_empty());
    assert_eq!(rows[1].tenant, "tenant-b");
    assert_eq!(rows[1].module_name, "mod-b");
    assert_eq!(rows[1].sha256_hash, "hash-b");
    assert_eq!(rows[1].size_bytes, 7);
    assert!(!rows[1].updated_at.is_empty());

    let metadata_row = store
        .load_wasm_module("tenant-a", "mod-a")
        .await
        .expect("load wasm metadata row")
        .expect("metadata row should exist");
    assert!(
        metadata_row.wasm_bytes.is_empty(),
        "Turso rows stay metadata-only; artifact bytes live in object storage"
    );
}
