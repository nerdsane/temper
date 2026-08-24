use std::fs;

use super::*;

#[test]
fn output_path_aliases_fail_before_writing() {
    let (temp, root, apps) = fixture();
    let original_manifest = fs::read(root.join("app.toml")).unwrap();

    let mut manifest_alias = inputs(&root, &apps);
    manifest_alias.source_out = Some("app.toml".into());
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: manifest_alias,
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("must be distinct paths"), "{error}");
    assert_eq!(fs::read(root.join("app.toml")).unwrap(), original_manifest);

    let mut output_alias = inputs(&root, &apps);
    output_alias.source_out = Some("same-output".into());
    output_alias.lock = Some("same-output".into());
    let error = generate_module_sdk(GenerateModuleSdkRequest {
        inputs: output_alias,
        check: false,
    })
    .unwrap_err();
    assert!(error.contains("must be distinct paths"), "{error}");

    generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap();
    let unbound = root.join("compiled.wasm");
    fs::write(&unbound, b"\0asm\x01\0\0\0").unwrap();
    for bound_wasm_out in ["app.toml", "temper-module-sdk.lock", "compiled.wasm"] {
        let error = bind_module_sdk(BindModuleSdkRequest {
            inputs: inputs(&root, &apps),
            wasm: unbound.clone(),
            bound_wasm_out: Some(bound_wasm_out.into()),
            check: false,
        })
        .unwrap_err();
        assert!(error.contains("must be distinct paths"), "{error}");
    }

    drop(temp);
}

#[test]
fn oversized_compiled_wasm_fails_before_allocation() {
    let (temp, root, apps) = fixture();
    generate_module_sdk(GenerateModuleSdkRequest {
        inputs: inputs(&root, &apps),
        check: false,
    })
    .unwrap();
    let unbound = temp.path().join("oversized.wasm");
    let file = fs::File::create(&unbound).unwrap();
    file.set_len(WASM_ARTIFACT_BYTES_BUDGET_V1 + 1).unwrap();
    let error = bind_module_sdk(BindModuleSdkRequest {
        inputs: inputs(&root, &apps),
        wasm: unbound,
        bound_wasm_out: None,
        check: false,
    })
    .unwrap_err();
    assert!(
        error.contains("compiled WASM byte budget exceeded"),
        "{error}"
    );
}

#[test]
fn manifest_publish_failure_restores_prior_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let artifact = temp.path().join("worker.wasm");
    let manifest = temp.path().join("app.toml");
    fs::write(&artifact, b"prior artifact").unwrap();
    fs::write(&manifest, b"prior manifest").unwrap();
    let mut publication = 0_usize;
    let error = publish_binding_with(
        &artifact,
        b"new artifact",
        &manifest,
        b"new manifest",
        |staged, path| {
            publication += 1;
            if publication == 2 {
                return Err("injected manifest publication failure".into());
            }
            persist_staged(staged, path)
        },
    )
    .unwrap_err();
    assert!(error.contains("injected manifest publication failure"));
    assert_eq!(fs::read(&artifact).unwrap(), b"prior artifact");
    assert_eq!(fs::read(&manifest).unwrap(), b"prior manifest");
}
