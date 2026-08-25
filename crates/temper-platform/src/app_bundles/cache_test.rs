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

const ROOT_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Example.Root" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Task">
        <Key><PropertyRef Name="id"/></Key>
        <Property Name="id" Type="Edm.String" Nullable="false"/>
        <Property Name="state" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Default">
        <EntitySet Name="Tasks" EntityType="Example.Root.Task"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

const DEPENDENCY_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Paw.FS" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File">
        <Key><PropertyRef Name="id"/></Key>
        <Property Name="id" Type="Edm.String" Nullable="false"/>
        <Property Name="state" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Default">
        <EntitySet Name="Files" EntityType="Paw.FS.File"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

fn ioa(name: &str) -> String {
    format!(
        r#"[automaton]
name = "{name}"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "Complete"
kind = "input"
from = ["Open"]
to = "Done"
"#
    )
}

fn bound_dependency_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let apps = temp.path().join("apps");
    let root = apps.join("root");
    let dependency = apps.join("dependency");
    std::fs::create_dir_all(root.join("wasm/worker/src")).unwrap();
    std::fs::create_dir_all(root.join("specs")).unwrap();
    std::fs::create_dir_all(dependency.join("specs")).unwrap();
    std::fs::write(
        root.join("app.toml"),
        r#"name = "root"
version = "1.0.0"
dependencies = ["dependency"]

[[wasm_modules]]
name = "worker"
target = "wasm32-wasip1"

[wasm_modules.data]
operations = ["entity_get"]

[[wasm_modules.data.entities]]
type = "Paw.FS.File"
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("temper.lock.toml"),
        "version = 1\n\n[[local]]\nname = \"dependency\"\npath = \"../dependency\"\n",
    )
    .unwrap();
    std::fs::write(root.join("specs/model.csdl.xml"), ROOT_CSDL).unwrap();
    std::fs::write(root.join("specs/task.ioa.toml"), ioa("Task")).unwrap();
    std::fs::write(
        dependency.join("app.toml"),
        "name = \"dependency\"\nversion = \"2.0.0\"\n",
    )
    .unwrap();
    std::fs::write(dependency.join("specs/model.csdl.xml"), DEPENDENCY_CSDL).unwrap();
    std::fs::write(dependency.join("specs/file.ioa.toml"), ioa("File")).unwrap();

    let inputs = crate::module_sdk_build::LocalModuleSdkInputs {
        app: root.clone(),
        module: "worker".to_string(),
        dependency_roots: vec![apps],
        app_manifest: None,
        source_out: None,
        lock: None,
    };
    crate::module_sdk_build::generate_module_sdk(
        crate::module_sdk_build::GenerateModuleSdkRequest {
            inputs: inputs.clone(),
            check: false,
        },
    )
    .unwrap();
    let unbound = temp.path().join("worker.wasm");
    std::fs::write(&unbound, b"\0asm\x01\0\0\0").unwrap();
    crate::module_sdk_build::bind_module_sdk(crate::module_sdk_build::BindModuleSdkRequest {
        inputs,
        wasm: unbound,
        bound_wasm_out: None,
        check: false,
    })
    .unwrap();
    (temp, root, dependency)
}

fn locked_workspace_bundle(
    root: &std::path::Path,
    tenant: &str,
) -> crate::app_bundles::WorkspaceBundle {
    let unlocked = super::super::workspace::build_workspace_bundle(root, tenant, false).unwrap();
    super::super::workspace::write_workspace_lock(&unlocked).unwrap();
    super::super::workspace::build_workspace_bundle(root, tenant, true).unwrap()
}

async fn bundle_test_platform(data_dir: std::path::PathBuf) -> crate::state::PlatformState {
    std::fs::create_dir_all(&data_dir).unwrap();
    let database_url = format!("file:{}", data_dir.join("metadata.db").display());
    let store = temper_store_turso::TursoEventStore::new(&database_url, None)
        .await
        .unwrap();
    let mut platform = crate::state::PlatformState::new(None);
    platform.server.data_dir = data_dir;
    platform
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(store));
    platform
}

#[tokio::test]
async fn locked_install_uses_dependency_metadata_lock_and_restores_without_sources() {
    let (temp, root, dependency) = bound_dependency_fixture();
    let locked = locked_workspace_bundle(&root, "typed-dependency");
    let manifest = crate::os_apps::read_app_manifest(&root).unwrap();
    let module_digest = manifest.wasm_modules[0]
        .data_binding
        .as_ref()
        .unwrap()
        .closure_digest
        .clone();
    assert_ne!(module_digest, locked.request.manifest.bundle_digest);

    let platform = bundle_test_platform(temp.path().join("data")).await;
    install_local_bundle(&platform, locked.request)
        .await
        .unwrap();

    std::fs::remove_dir_all(&root).unwrap();
    std::fs::remove_dir_all(&dependency).unwrap();
    assert_eq!(
        restore_local_bundle_cache_roots(&platform).await.unwrap(),
        1
    );
}

#[tokio::test]
async fn locked_install_rejects_dependency_metadata_changed_after_binding() {
    let (temp, root, dependency) = bound_dependency_fixture();
    std::fs::write(
        dependency.join("specs/model.csdl.xml"),
        DEPENDENCY_CSDL.replace(
            "</EntityType>",
            "<Property Name=\"path\" Type=\"Edm.String\"/></EntityType>",
        ),
    )
    .unwrap();
    let locked = locked_workspace_bundle(&root, "stale-dependency");
    let platform = bundle_test_platform(temp.path().join("data")).await;
    let error = install_local_bundle(&platform, locked.request)
        .await
        .unwrap_err();
    assert!(
        error.contains("differs without an artifact-bound proof"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn locked_install_rejects_entity_available_only_in_ambient_tenant_schema() {
    let (temp, root, dependency) = bound_dependency_fixture();
    let platform = bundle_test_platform(temp.path().join("data")).await;
    let ambient =
        super::super::workspace::build_workspace_bundle(&dependency, "ambient-schema", false)
            .unwrap();
    install_local_bundle(&platform, ambient.request)
        .await
        .unwrap();

    let manifest_path = root.join("app.toml");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    std::fs::write(
        &manifest_path,
        manifest.replace("dependencies = [\"dependency\"]\n", "dependencies = []\n"),
    )
    .unwrap();
    std::fs::write(root.join("temper.lock.toml"), "version = 1\n").unwrap();
    let root_only =
        super::super::workspace::build_workspace_bundle(&root, "ambient-schema", true).unwrap();
    let error = install_local_bundle(&platform, root_only.request)
        .await
        .unwrap_err();
    assert!(
        error.contains("granted entity type 'Paw.FS.File' is absent"),
        "unexpected error: {error}"
    );
}
