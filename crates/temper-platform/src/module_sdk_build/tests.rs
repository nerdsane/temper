use std::fs;

use temper_wasm_sdk::data::read_module_sdk_artifact_binding;
use tempfile::TempDir;

use super::*;

mod safety;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Task">
        <Key><PropertyRef Name="id"/></Key>
        <Property Name="id" Type="Edm.String" Nullable="false"/>
        <Property Name="state" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Default">
        <EntitySet Name="Tasks" EntityType="Example.Task"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

const IOA: &str = r#"[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "Complete"
kind = "input"
from = ["Open"]
to = "Done"
"#;

fn root_manifest(dependency: &str) -> String {
    format!(
        r#"name = "root"
version = "1.0.0"
dependencies = ["{dependency}"]

[[wasm_modules]]
name = "worker"
target = "wasm32-wasip1"

[wasm_modules.data]
operations = ["entity_get", "entity_query"]

[[wasm_modules.data.entities]]
type = "Example.Task"
query_order_by_sequence = true
"#
    )
}

fn fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let apps = temp.path().join("apps");
    let root = apps.join("root");
    let dependency = apps.join("dependency");
    fs::create_dir_all(root.join("wasm/worker/src")).unwrap();
    fs::create_dir_all(dependency.join("specs")).unwrap();
    fs::write(root.join("app.toml"), root_manifest("dependency")).unwrap();
    fs::write(
        dependency.join("app.toml"),
        "name = \"dependency\"\nversion = \"2.0.0\"\n",
    )
    .unwrap();
    fs::write(dependency.join("specs/model.csdl.xml"), CSDL).unwrap();
    fs::write(dependency.join("specs/task.ioa.toml"), IOA).unwrap();
    (temp, root, apps)
}

fn inputs(root: &std::path::Path, apps: &std::path::Path) -> LocalModuleSdkInputs {
    LocalModuleSdkInputs {
        app: root.into(),
        module: "worker".into(),
        dependency_roots: vec![apps.into()],
        app_manifest: None,
        source_out: None,
        lock: None,
    }
}

#[test]
fn generation_is_deterministic_and_check_detects_drift() {
    let (_temp, root, apps) = fixture();
    let request = GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    };
    let first = generate_module_sdk(request.clone()).unwrap();
    let source = fs::read(&first.source).unwrap();
    let lock = fs::read(&first.lock).unwrap();
    let second = generate_module_sdk(request).unwrap();
    assert_eq!(source, fs::read(&second.source).unwrap());
    assert_eq!(lock, fs::read(&second.lock).unwrap());
    assert_eq!(first.closure_digest, second.closure_digest);

    generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: true,
    })
    .unwrap();
    fs::write(&first.source, "stale").unwrap();
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: true,
    })
    .unwrap_err();
    assert!(error.contains("generated source drift"));
    assert_eq!(fs::read_to_string(first.source).unwrap(), "stale");
}

#[test]
fn metadata_changes_advance_the_lock_and_schema() {
    let (_temp, root, apps) = fixture();
    let first = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap();
    let first_source = fs::read_to_string(&first.source).unwrap();
    let changed = CSDL.replace(
        "</EntityType>",
        "<Property Name=\"Title\" Type=\"Edm.String\" Nullable=\"true\"/></EntityType>",
    );
    fs::write(apps.join("dependency/specs/model.csdl.xml"), changed).unwrap();
    let second = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap();
    assert_ne!(first.closure_digest, second.closure_digest);
    assert_ne!(first_source, fs::read_to_string(second.source).unwrap());
}

#[test]
fn bind_packages_wasm_updates_manifest_and_checks_cleanly() {
    let (temp, root, apps) = fixture();
    generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap();
    let unbound = temp.path().join("worker.wasm");
    fs::write(&unbound, b"\0asm\x01\0\0\0").unwrap();
    let request = BindModuleSdkRequest {
        inputs: inputs(&root, &apps),
        wasm: unbound,
        bound_wasm_out: None,
        check: false,
    };
    let report = bind_module_sdk(request.clone()).unwrap();
    let bound = fs::read(report.bound_wasm.as_ref().unwrap()).unwrap();
    let embedded = read_module_sdk_artifact_binding(&bound)
        .unwrap()
        .expect("binding custom section");
    assert_eq!(embedded.module_name, "worker");
    let manifest_source = fs::read_to_string(root.join("app.toml")).unwrap();
    assert!(manifest_source.contains("[wasm_modules.data_binding]"));
    let manifest: crate::os_apps::AppManifest = toml::from_str(&manifest_source).unwrap();
    manifest.validate().unwrap();

    let mut check = request;
    check.check = true;
    bind_module_sdk(check).unwrap();
}

#[test]
fn missing_dependencies_and_path_traversal_fail_closed() {
    let (_temp, root, apps) = fixture();
    fs::write(root.join("app.toml"), root_manifest("missing")).unwrap();
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("missing local dependency"));

    fs::write(root.join("app.toml"), root_manifest("dependency")).unwrap();
    let mut unsafe_inputs = inputs(&root, &apps);
    unsafe_inputs.source_out = Some(std::path::PathBuf::from("../escape.rs"));
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: unsafe_inputs,
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("must not contain '..'"));
}

#[cfg(unix)]
#[test]
fn metadata_symlinks_cannot_escape_an_app_root() {
    use std::os::unix::fs::symlink;

    let (temp, root, apps) = fixture();
    let csdl = apps.join("dependency/specs/model.csdl.xml");
    fs::remove_file(&csdl).unwrap();
    let outside = temp.path().join("outside.csdl.xml");
    fs::write(&outside, CSDL).unwrap();
    symlink(&outside, &csdl).unwrap();

    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("escapes app root"));
}

#[test]
fn only_declared_dependencies_are_loaded() {
    let (_temp, root, apps) = fixture();
    let unrelated = apps.join("unrelated");
    fs::create_dir_all(&unrelated).unwrap();
    fs::write(unrelated.join("app.toml"), "not valid toml = [").unwrap();

    generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap();
}

#[test]
fn ambiguous_and_cyclic_dependency_graphs_fail_closed() {
    let (temp, root, apps) = fixture();
    let other_root = temp.path().join("other-apps");
    let duplicate = other_root.join("dependency");
    fs::create_dir_all(&duplicate).unwrap();
    fs::write(
        duplicate.join("app.toml"),
        "name = \"dependency\"\nversion = \"3.0.0\"\n",
    )
    .unwrap();
    let mut ambiguous = inputs(&root, &apps);
    ambiguous.dependency_roots.push(other_root);
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: ambiguous,
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("ambiguous"));

    fs::write(
        apps.join("dependency/app.toml"),
        "name = \"dependency\"\nversion = \"2.0.0\"\ndependencies = [\"root\"]\n",
    )
    .unwrap();
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("cyclic local app dependency"));
}

#[test]
fn stale_lock_is_detected_independently_from_source_drift() {
    let (_temp, root, apps) = fixture();
    let report = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap();
    fs::write(&report.lock, "stale lock").unwrap();
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: true,
    })
    .unwrap_err();
    assert!(error.contains("module SDK lock drift"));
}

#[test]
fn already_bound_input_and_every_bind_check_drift_fail_closed() {
    let (temp, root, apps) = fixture();
    generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap();
    let unbound = temp.path().join("worker.wasm");
    fs::write(&unbound, b"\0asm\x01\0\0\0").unwrap();
    let request = BindModuleSdkRequest {
        inputs: inputs(&root, &apps),
        wasm: unbound.clone(),
        bound_wasm_out: None,
        check: false,
    };
    let report = bind_module_sdk(request.clone()).unwrap();
    let bound_path = report.bound_wasm.unwrap();

    let mut already_bound = request.clone();
    already_bound.wasm = bound_path.clone();
    let error = bind_module_sdk(already_bound).unwrap_err();
    assert!(error.contains("already exists"), "{error}");

    fs::write(&bound_path, b"drift").unwrap();
    let mut check = request.clone();
    check.check = true;
    let error = bind_module_sdk(check.clone()).unwrap_err();
    assert!(error.contains("bound WASM drift"));
    bind_module_sdk(request.clone()).unwrap();

    fs::write(&unbound, b"\0asm\x01\0\0\0\x00\x01\x00").unwrap();
    let error = bind_module_sdk(check.clone()).unwrap_err();
    assert!(error.contains("bound WASM drift"));
    fs::write(&unbound, b"\0asm\x01\0\0\0").unwrap();

    let manifest_path = root.join("app.toml");
    let manifest = fs::read_to_string(&manifest_path).unwrap();
    fs::write(
        &manifest_path,
        manifest.replace(
            "query_order_by_sequence = true",
            "query_order_by_sequence = false",
        ),
    )
    .unwrap();
    let error = bind_module_sdk(check.clone()).unwrap_err();
    assert!(error.contains("generated source drift") || error.contains("module SDK lock drift"));
    fs::write(&manifest_path, &manifest).unwrap();

    fs::write(
        &manifest_path,
        manifest.replace("artifact_digest =", "artifact_digest = \"tampered\" #"),
    )
    .unwrap();
    let error = bind_module_sdk(check).unwrap_err();
    assert!(error.contains("app manifest binding drift"));
}

#[test]
fn metadata_budgets_reject_oversized_manifests_and_closures() {
    let (_temp, root, apps) = fixture();
    fs::write(root.join("app.toml"), vec![b' '; 1024 * 1024 + 1]).unwrap();
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("app manifest byte budget exceeded"));

    fs::write(root.join("app.toml"), root_manifest("chain-000")).unwrap();
    for index in 0..128 {
        let name = format!("chain-{index:03}");
        let dir = apps.join(&name);
        fs::create_dir_all(&dir).unwrap();
        let dependencies = if index == 127 {
            String::new()
        } else {
            format!("dependencies = [\"chain-{:03}\"]\n", index + 1)
        };
        fs::write(
            dir.join("app.toml"),
            format!("name = \"{name}\"\nversion = \"1.0.0\"\n{dependencies}"),
        )
        .unwrap();
    }
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("closure budget exceeded"));

    for index in 0..1_025 {
        let dir = apps.join(format!("candidate-{index:04}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("app.toml"),
            format!("name = \"candidate-{index:04}\"\n"),
        )
        .unwrap();
    }
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("dependency candidate budget exceeded"));
}

#[test]
fn mixed_csdl_locations_are_rejected_as_ambiguous() {
    let (_temp, root, apps) = fixture();
    fs::create_dir_all(root.join("specs")).unwrap();
    fs::write(root.join("model.csdl.xml"), CSDL).unwrap();
    fs::write(root.join("specs/model.csdl.xml"), CSDL).unwrap();
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("multiple CSDL candidates"));
}

#[test]
fn incompatible_actions_functions_and_imports_fail_closed() {
    let variants = [
        (
            "<Action Name=\"Resolve\"><Parameter Name=\"value\" Type=\"Edm.String\"/></Action>",
            "<Action Name=\"Resolve\"><Parameter Name=\"value\" Type=\"Edm.Int32\"/></Action>",
        ),
        (
            "<Function Name=\"Resolve\"><ReturnType Type=\"Edm.String\"/></Function>",
            "<Function Name=\"Resolve\"><ReturnType Type=\"Edm.Int32\"/></Function>",
        ),
    ];
    for (root_symbol, dependency_symbol) in variants {
        let (_temp, root, apps) = fixture();
        fs::write(
            root.join("model.csdl.xml"),
            CSDL.replace(
                "<EntityContainer",
                &format!("{root_symbol}<EntityContainer"),
            ),
        )
        .unwrap();
        fs::write(
            apps.join("dependency/specs/model.csdl.xml"),
            CSDL.replace(
                "<EntityContainer",
                &format!("{dependency_symbol}<EntityContainer"),
            ),
        )
        .unwrap();
        let error = generate_module_sdk(GenerateModuleSdkRequest {
            inputs: inputs(&root, &apps),
            check: false,
        })
        .unwrap_err();
        assert!(error.contains("schema conflict"));
    }

    let (_temp, root, apps) = fixture();
    let import = |action: &str| {
        CSDL.replace(
            "</EntityContainer>",
            &format!("<ActionImport Name=\"Resolve\" Action=\"{action}\"/></EntityContainer>"),
        )
    };
    fs::write(root.join("model.csdl.xml"), import("Example.One")).unwrap();
    fs::write(
        apps.join("dependency/specs/model.csdl.xml"),
        import("Example.Two"),
    )
    .unwrap();
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("schema conflict"));
}

#[test]
fn identical_record_annotations_do_not_create_false_conflicts() {
    let (_temp, root, apps) = fixture();
    let annotated = CSDL.replace(
        "</EntityType>",
        "<Annotation Term=\"Example.Metadata\"><Record><PropertyValue Property=\"Zulu\" String=\"last\"/><PropertyValue Property=\"Alpha\" String=\"first\"/></Record></Annotation></EntityType>",
    );
    fs::write(root.join("model.csdl.xml"), &annotated).unwrap();
    fs::write(apps.join("dependency/specs/model.csdl.xml"), annotated).unwrap();

    generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap();
}
