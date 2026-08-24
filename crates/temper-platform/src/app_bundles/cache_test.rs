use super::super::workspace::digest_manifest_records;
use super::*;

#[test]
fn manifest_rejects_digest_mismatch_and_unsafe_paths() {
    let app = super::super::types::CanonicalAppManifest {
        name: "sample".to_string(),
        version: "1.0.0".to_string(),
        dependencies: Vec::new(),
        files: vec![super::super::types::CanonicalFileManifest {
            path: "../escape".to_string(),
            size: 0,
            blob_digest: sha256_prefixed(&[]),
        }],
    };
    let manifest = CanonicalBundleManifestV1 {
        schema_version: 1,
        root_app: "sample".to_string(),
        apps: vec![app],
        bundle_digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_string(),
    };
    assert!(validate_manifest(&manifest).is_err());
}

#[test]
fn cached_bundle_restore_revalidates_blob_content() {
    let data = tempfile::tempdir().unwrap();
    let bytes = b"name = \"sample\"\nversion = \"1.0.0\"\n";
    let blob_digest = sha256_prefixed(bytes);
    let app = super::super::types::CanonicalAppManifest {
        name: "sample".to_string(),
        version: "1.0.0".to_string(),
        dependencies: Vec::new(),
        files: vec![super::super::types::CanonicalFileManifest {
            path: "app.toml".to_string(),
            size: bytes.len() as u64,
            blob_digest: blob_digest.clone(),
        }],
    };
    let manifest = CanonicalBundleManifestV1 {
        schema_version: 1,
        root_app: "sample".to_string(),
        bundle_digest: digest_manifest_records("sample", std::slice::from_ref(&app)),
        apps: vec![app],
    };
    let transported = vec![BundleBlob {
        digest: blob_digest.clone(),
        content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    }];
    publish_bundle(data.path(), &manifest, &transported).unwrap();

    let path = blob_path(&cache_root(data.path()), &blob_digest).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"corrupt").unwrap();

    let error = materialize_cached_bundle(data.path(), &manifest.bundle_digest).unwrap_err();
    assert!(error.contains("integrity"), "unexpected error: {error}");
}

#[tokio::test]
async fn same_app_name_is_pinned_independently_per_tenant() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    for (path, version) in [(&first, "1.0.0"), (&second, "2.0.0")] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("app.toml"),
            format!("name = \"shared\"\nversion = \"{version}\"\n"),
        )
        .unwrap();
        std::fs::write(path.join("APP.md"), format!("# Shared {version}\n")).unwrap();
    }
    let first_bundle =
        super::super::workspace::build_workspace_bundle(&first, "tenant-a", false).unwrap();
    let second_bundle =
        super::super::workspace::build_workspace_bundle(&second, "tenant-b", false).unwrap();
    let first_pin = first_bundle.request.manifest.bundle_digest.clone();
    let second_pin = second_bundle.request.manifest.bundle_digest.clone();

    let database_url = format!("file:{}", data_dir.join("metadata.db").display());
    let store = temper_store_turso::TursoEventStore::new(&database_url, None)
        .await
        .unwrap();
    let mut platform = crate::state::PlatformState::new(None);
    platform.server.data_dir = data_dir;
    platform
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(store));
    install_local_bundle(&platform, first_bundle.request)
        .await
        .unwrap();
    install_local_bundle(&platform, second_bundle.request)
        .await
        .unwrap();

    let durable = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
        .unwrap();
    let first_record = durable
        .get_installed_app("tenant-a", "shared")
        .await
        .unwrap()
        .unwrap();
    let second_record = durable
        .get_installed_app("tenant-b", "shared")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_record.version_hash, first_pin);
    assert_eq!(second_record.version_hash, second_pin);
    assert_ne!(first_record.bundle_digest, second_record.bundle_digest);

    std::fs::remove_dir_all(first).unwrap();
    std::fs::remove_dir_all(second).unwrap();
    assert_eq!(
        restore_local_bundle_cache_roots(&platform).await.unwrap(),
        2
    );
}
