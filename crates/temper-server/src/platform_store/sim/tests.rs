//! Simulated atomic-publication regression tests.

use super::*;

#[tokio::test]
async fn atomic_publication_commits_specs_and_wasm_before_ambiguous_error() {
    let store = SimPlatformStore::no_faults(238);
    store.fail_next_spec_publications_after_commit(1);
    let spec = SpecPublication {
        entity_type: "Widget",
        ioa_source: "[automaton]\nname = \"Widget\"",
        csdl_xml: "<edmx />",
        content_hash: "spec-hash",
    };
    let wasm = WasmPublication {
        module_name: "widget_hook",
        wasm_bytes: b"\0asm\x01\0\0\0",
        sha256_hash: "wasm-hash",
        source: "bundled",
    };

    let error = store
        .publish_specs(
            "default",
            &[spec],
            SpecPublicationMode::Merge,
            TenantConstraintsPublication::Preserve,
            TenantPolicyPublication::Preserve,
            None,
            None,
            &[wasm],
        )
        .await
        .expect_err("post-commit acknowledgement must be ambiguous");
    assert!(error.contains("post-commit"));
    assert_eq!(store.load_specs().await.expect("load specs").len(), 1);
    let modules = store
        .load_all_wasm_modules("default")
        .await
        .expect("load WASM generation");
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].sha256_hash, "wasm-hash");
    assert_eq!(modules[0].source, "bundled");
}

#[tokio::test]
async fn atomic_publication_matches_uploaded_wasm_source_precedence() {
    let store = SimPlatformStore::no_faults(239);
    store
        .upsert_wasm_module("default", "hook", b"upload", "upload-hash", "upload")
        .await
        .expect("seed uploaded WASM");
    let bundled = WasmPublication {
        module_name: "hook",
        wasm_bytes: b"bundled",
        sha256_hash: "bundled-hash",
        source: "bundled",
    };
    store
        .publish_specs(
            "default",
            &[],
            SpecPublicationMode::Merge,
            TenantConstraintsPublication::Preserve,
            TenantPolicyPublication::Preserve,
            None,
            None,
            &[bundled],
        )
        .await
        .expect("plain bundle publication");
    let preserved = store
        .load_all_wasm_modules("default")
        .await
        .expect("load preserved upload");
    assert_eq!(preserved[0].sha256_hash, "upload-hash");
    assert_eq!(preserved[0].source, "upload");

    let replacement = WasmPublication {
        source: "bundled-replace-upload",
        ..bundled
    };
    store
        .publish_specs(
            "default",
            &[],
            SpecPublicationMode::Merge,
            TenantConstraintsPublication::Preserve,
            TenantPolicyPublication::Preserve,
            None,
            None,
            &[replacement],
        )
        .await
        .expect("explicit bundle replacement");
    let replaced = store
        .load_all_wasm_modules("default")
        .await
        .expect("load replaced bundle");
    assert_eq!(replaced[0].sha256_hash, "bundled-hash");
    assert_eq!(replaced[0].source, "bundled");
}
