use super::*;

#[tokio::test]
async fn upsert_wasm_module_preserves_version_for_identical_hash() {
    let store = make_store("wasm-idempotent").await;

    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a", "bundled")
        .await
        .expect("initial wasm upsert");
    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a", "bundled")
        .await
        .expect("identical wasm upsert");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT version FROM wasm_modules WHERE tenant = ?1 AND module_name = ?2",
            params!["tenant-a", "mod-a"],
        )
        .await
        .expect("query wasm version");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("wasm row exists");
    let version: i64 = row.get(0).expect("version");

    assert_eq!(version, 1, "identical WASM hash must not bump version");
}

#[tokio::test]
async fn upsert_wasm_module_stores_metadata_only_without_db_blob() {
    let store = make_store("wasm-artifact").await;

    store
        .upsert_wasm_module("tenant-a", "mod-a", b"hello-a", "hash-a", "bundled")
        .await
        .expect("persist wasm artifact");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT length(wasm_bytes) FROM wasm_modules WHERE tenant = ?1 AND module_name = ?2",
            params!["tenant-a", "mod-a"],
        )
        .await
        .expect("query inline wasm length");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("wasm row exists");
    let inline_len: i64 = row.get(0).expect("inline wasm length");

    assert_eq!(
        inline_len, 0,
        "new WASM metadata rows should point at artifact storage, not inline bytes"
    );

    let artifact = store
        .get_blob("wasm-modules/hash-a")
        .await
        .expect("query legacy db blob");
    assert!(
        artifact.is_none(),
        "new WASM artifacts must not create Turso blob rows"
    );

    let loaded = store
        .load_wasm_module("tenant-a", "mod-a")
        .await
        .expect("load wasm row")
        .expect("wasm row exists");
    assert!(
        loaded.wasm_bytes.is_empty(),
        "Turso store should return metadata-only rows for new WASM artifacts"
    );
}
