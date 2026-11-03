use temper_runtime::ActorSystem;
use temper_spec::csdl::CsdlDocument;

use super::*;

#[test]
fn tenant_object_namespaces_are_stable_and_default_compatible() {
    assert_eq!(tenant_object_namespace(&TenantId::default()), None);
    let namespace =
        tenant_object_namespace(&TenantId::new("tenant-a")).expect("non-default tenant namespace");
    assert_eq!(namespace.len(), 64);
    assert_eq!(
        tenant_object_namespace(&TenantId::new("tenant-a")),
        Some(namespace)
    );
}

#[test]
fn a_deployment_prefix_comes_before_the_tenant_namespace() {
    let other = TenantId::new("tenant-a");
    let namespace = tenant_object_namespace(&other).expect("namespace");
    assert_eq!(object_key_prefix(None, &TenantId::default()), None);
    assert_eq!(
        object_key_prefix(Some("/people/t1/a1/"), &TenantId::default()).as_deref(),
        Some("people/t1/a1")
    );
    assert_eq!(object_key_prefix(Some(""), &TenantId::default()), None);
    assert_eq!(
        object_key_prefix(None, &other),
        Some(format!("tenants/{namespace}"))
    );
    assert_eq!(
        object_key_prefix(Some("people/t1"), &other),
        Some(format!("people/t1/tenants/{namespace}"))
    );
}

#[tokio::test]
async fn a_caller_s_blob_is_never_written_under_the_kernel_s_own_keys() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let mut state = ServerState::new(
        ActorSystem::new("kernel-blob-keys"),
        CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    );
    state.data_dir = data_dir.path().to_path_buf();
    let tenant = TenantId::new("tenant-a");
    let forged = br#"{"all_passed":true}"#;

    for key in ["kernel/check-verdict/abc", "wasm-modules/abc"] {
        let refused = state.put_blob_object(&tenant, key, forged, None).await;
        assert!(refused.is_err(), "{key}: {refused:?}");
        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&tenant, key)
                .await
                .expect("read"),
            None,
            "{key}"
        );
    }
    state
        .put_blob_object(&tenant, "check-verdict/abc", forged, None)
        .await
        .expect("a key outside the kernel's is written");
}

#[tokio::test]
async fn local_object_storage_is_namespaced_by_tenant() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let mut state = ServerState::new(
        ActorSystem::new("tenant-blob-isolation"),
        CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    );
    state.data_dir = data_dir.path().to_path_buf();
    let tenant_a = TenantId::new("tenant-a");
    let tenant_b = TenantId::new("tenant-b");

    state
        .put_blob_object(&tenant_a, "field-overflow/value", b"tenant-a", None)
        .await
        .expect("tenant A write");
    state
        .put_blob_object(&tenant_b, "field-overflow/value", b"tenant-b", None)
        .await
        .expect("tenant B write");

    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&tenant_a, "field-overflow/value")
            .await
            .expect("tenant A read"),
        Some(b"tenant-a".to_vec())
    );
    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&tenant_b, "field-overflow/value")
            .await
            .expect("tenant B read"),
        Some(b"tenant-b".to_vec())
    );
    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&TenantId::new("tenant-c"), "field-overflow/value",)
            .await
            .expect("tenant C read"),
        None
    );
}

/// A Katagami app whose vault key was lost (Run probe, 21 Oct 2026) restored its data with no readable
/// `blob_endpoint`, and a File put then answered 204 while its bytes went to this disk and the app's database.
#[tokio::test]
async fn a_server_that_keeps_files_only_in_an_object_store_refuses_them_without_one() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let mut state = ServerState::new(
        ActorSystem::new("files-need-object-store"),
        CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    );
    state.data_dir = data_dir.path().to_path_buf();
    let tenant = TenantId::default();
    let key = "temper-fs/sha256:refused";

    state.files_need_object_store = true;
    let refused = state
        .put_file_content_blob(&tenant, key, b"bytes")
        .await
        .expect_err("no object store, so no File bytes");
    assert!(refused.contains("only in an object store"), "{refused}");
    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&tenant, key)
            .await
            .expect("read"),
        None,
        "nothing kept on disk or in the database"
    );

    // Without the setting, a local store keeps them as before.
    state.files_need_object_store = false;
    state
        .put_file_content_blob(&tenant, key, b"bytes")
        .await
        .expect("a local store keeps Files");
    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&tenant, key)
            .await
            .expect("read"),
        Some(b"bytes".to_vec())
    );
}
