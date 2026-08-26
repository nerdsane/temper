use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;
use temper_server::EntityState;
use temper_wasm_sdk::data::{
    ManifestEntityV1, ManifestPropertyV1, ModuleDataGrant, ModuleSdkManifest,
    ModuleSdkMetadataDigests,
};

use super::reconcile;
use crate::state::PlatformState;

#[test]
fn workspace_free_restart_preserves_canonical_default_behavior() {
    let state = PlatformState::new(None);
    let tenant = TenantId::new("cache-restart");
    let wasm_bytes = b"registered-wasm-artifact".to_vec();
    let artifact_digest = temper_wasm::WasmEngine::hash_module(&wasm_bytes);
    state.server.wasm_module_registry.write().unwrap().register(
        &tenant,
        "worker",
        &artifact_digest,
    );
    let binding = ModuleSdkManifest::new(
        "worker",
        ModuleSdkMetadataDigests {
            closure: "closure".into(),
            dependency_lock: "closure".into(),
            schema: "schema".into(),
        },
        &artifact_digest,
        ModuleDataGrant::default(),
        vec![ManifestEntityV1 {
            entity_type: "Temper.Example.Customer".into(),
            entity_set: "Customers".into(),
            generated_name: "Customer".into(),
            properties: vec![ManifestPropertyV1 {
                canonical_name: "FailureReason".into(),
                generated_name: "failure_reason".into(),
                type_name: "Edm.String".into(),
                nullable: false,
                default_value: Some(serde_json::json!("")),
                enum_members: Vec::new(),
            }],
            actions: Vec::new(),
        }],
        BTreeSet::new(),
    )
    .expect("valid binding");
    let entity_state: EntityState = serde_json::from_value(serde_json::json!({
        "entity_type": "Customer",
        "entity_id": "customer-1",
        "status": "Active",
        "item_count": 0,
        "fields": {},
        "events": []
    }))
    .expect("sparse committed state");
    let before = temper_server::application_data::canonicalize_entity_for_test(
        &binding.entities[0],
        &entity_state,
    )
    .expect("pre-restart canonical response");
    let digest_before = binding.binding_digest();

    let binding: ModuleSdkManifest =
        serde_json::from_slice(&serde_json::to_vec(&binding).expect("locked binding serializes"))
            .expect("locked binding restores without workspace sources");
    let wasm_modules = BTreeMap::from([("worker".to_string(), wasm_bytes)]);
    let canonical_bindings = BTreeMap::from([("worker".to_string(), binding)]);
    reconcile::restore_canonical_data_bindings(
        &state,
        "cache-restart",
        &wasm_modules,
        &canonical_bindings,
    )
    .expect("registered artifact should be rebound");

    let registry = state.server.wasm_module_registry.read().unwrap();
    let restored = registry
        .data_manifest(&tenant, "worker", &artifact_digest)
        .expect("cache recovery restores verified typed-data binding");
    let after = temper_server::application_data::canonicalize_entity_for_test(
        &restored.entities[0],
        &entity_state,
    )
    .expect("post-restart canonical response");
    assert_eq!(restored.binding_digest(), digest_before);
    assert_eq!(after, before);
    assert_eq!(after.get("FailureReason"), Some(&serde_json::json!("")));
}
