use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;
use temper_server::EntityState;
use temper_wasm_sdk::data::{
    DataOperationKind, EntityDataGrant, ModuleDataGrant, ModuleSdkManifest,
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
    let csdl = temper_spec::csdl::parse_csdl(
        r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Temper.Example" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Customer"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="State" Type="Edm.String" Nullable="false" DefaultValue="Unconfigured"/><Property Name="FailureReason" Type="Edm.String" Nullable="false" DefaultValue=""/></EntityType><EntityContainer Name="Container"><EntitySet Name="Customers" EntityType="Temper.Example.Customer"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#,
    )
    .expect("restart fixture CSDL parses");
    let binding = temper_codegen::generate_module_sdk(
        &csdl,
        &[temper_spec::bundle::IoaSourceInput {
            entity_type: "Temper.Example.Customer".into(),
            source: r#"[automaton]
name = "Customer"
states = ["Unconfigured", "Active"]
initial = "Unconfigured"

[[action]]
name = "Activate"
kind = "input"
from = ["Unconfigured"]
to = "Active"
"#
            .into(),
        }],
        "worker",
        "closure",
        "closure",
        &artifact_digest,
        ModuleDataGrant {
            operations: BTreeSet::from([DataOperationKind::EntityGet]),
            entities: vec![EntityDataGrant {
                entity_type: "Temper.Example.Customer".into(),
                ..EntityDataGrant::default()
            }],
            ..ModuleDataGrant::default()
        },
    )
    .expect("valid generated binding")
    .manifest;
    let entity_state: EntityState = serde_json::from_value(serde_json::json!({
        "entity_type": "Customer",
        "entity_id": "customer-1",
        "status": "Active",
        "item_count": 0,
        "fields": {"State": "Unconfigured"},
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
    assert_eq!(after.get("State"), Some(&serde_json::json!("Active")));
    assert_eq!(after.get("FailureReason"), Some(&serde_json::json!("")));
}
