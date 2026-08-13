use super::*;
use temper_spec::csdl::parse_csdl;
use temper_wasm_sdk::data::{DataOperationKind, EntityDataGrant};

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices><Schema Namespace="Temper.App" xmlns="http://docs.oasis-open.org/odata/ns/edm">
    <EntityType Name="Task"><Key><PropertyRef Name="Id"/></Key>
      <Property Name="Id" Type="Edm.String" Nullable="false"/>
      <Property Name="Status" Type="Edm.String" Nullable="false"/>
    </EntityType>
    <Action Name="StartWork" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/></Action>
    <EntityContainer Name="Container"><EntitySet Name="Tasks" EntityType="Temper.App.Task"/></EntityContainer>
  </Schema></edmx:DataServices>
</edmx:Edmx>"#;

fn grant() -> ModuleDataGrant {
    let mut grant = ModuleDataGrant::default();
    grant.operations.insert(DataOperationKind::EntityGet);
    grant.operations.insert(DataOperationKind::ActionInvoke);
    let mut entity = EntityDataGrant {
        entity_type: "Temper.App.Task".into(),
        ..EntityDataGrant::default()
    };
    entity.actions.insert("StartWork".into());
    entity.query_filter_fields.insert("Status".into());
    grant.entities.push(entity);
    grant
}

#[test]
fn generation_is_deterministic_and_scoped() {
    let csdl = parse_csdl(CSDL).unwrap();
    let first =
        generate_module_sdk(&csdl, "worker", "closure", "closure", "artifact", grant()).unwrap();
    let second =
        generate_module_sdk(&csdl, "worker", "closure", "closure", "artifact", grant()).unwrap();
    assert_eq!(first.source, second.source);
    assert_eq!(first.manifest, second.manifest);
    assert!(first.source.contains("TaskClient"));
    assert!(!first.source.contains("EntityPatch"));
    assert!(first.source.contains("pub status: String"));
    assert!(first.source.contains("pub fn start_work"));
    assert!(first.source.contains("Result<TypedEntity<Task>"));
    assert!(first.source.contains("TEMPER_MODULE_SCHEMA_DIGEST"));
    assert!(first.source.contains("TEMPER_MODULE_USED_SYMBOLS_DIGEST"));
    first.manifest.verify_binding().unwrap();
}

#[test]
fn generated_names_preserve_word_boundaries_and_escape_keywords() {
    assert_eq!(rust_field_name("CreatedAt"), "created_at");
    assert_eq!(rust_field_name("created_at"), "created_at");
    assert_eq!(rust_field_name("type"), "type_");
    assert_eq!(rust_field_name("gen"), "gen_");
}

#[test]
fn methods_are_not_emitted_without_global_operation_grants() {
    let csdl = parse_csdl(CSDL).unwrap();
    let mut scoped = grant();
    scoped.operations.remove(&DataOperationKind::EntityGet);
    scoped.operations.remove(&DataOperationKind::ActionInvoke);
    let generated =
        generate_module_sdk(&csdl, "worker", "closure", "closure", "artifact", scoped).unwrap();
    assert!(!generated.source.contains("pub fn get("));
    assert!(!generated.source.contains("pub fn start_work("));
}

#[test]
fn unknown_granted_symbol_fails_closed() {
    let csdl = parse_csdl(CSDL).unwrap();
    let mut invalid = grant();
    invalid.entities[0]
        .actions
        .insert("DeleteEverything".into());
    assert!(matches!(
        generate_module_sdk(&csdl, "worker", "closure", "closure", "artifact", invalid),
        Err(ModuleSdkCodegenError::MissingSymbol { .. })
    ));
}

#[test]
fn packaging_binds_manifest_into_exact_artifact() {
    let csdl = parse_csdl(CSDL).unwrap();
    let generated =
        generate_module_sdk(&csdl, "worker", "closure", "closure", "", grant()).unwrap();
    let packaged = package_generated_module_sdk(b"\0asm\x01\0\0\0", generated).unwrap();
    assert_eq!(
        packaged.manifest.artifact_digest,
        hex_sha256(&packaged.wasm)
    );
    let embedded = temper_wasm_sdk::data::read_module_sdk_artifact_binding(&packaged.wasm)
        .unwrap()
        .unwrap();
    assert_eq!(embedded.module_name, "worker");
    assert_eq!(embedded.grant_digest, packaged.manifest.grant_digest);
}
