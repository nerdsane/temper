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
    <EntityType Name="Receipt"><Key><PropertyRef Name="Id"/></Key>
      <Property Name="Id" Type="Edm.String" Nullable="false"/>
      <Property Name="Status" Type="Edm.String" Nullable="false"/>
    </EntityType>
    <EnumType Name="Outcome"><Member Name="Accepted"/><Member Name="Rejected"/></EnumType>
    <Action Name="StartWork" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Temper.App.Task" Nullable="false"/></Action>
    <Action Name="MaybeStart" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Temper.App.Task" Nullable="true"/></Action>
    <Action Name="AttemptCount" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Edm.Int32" Nullable="false"/></Action>
    <Action Name="Outcome" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Temper.App.Outcome" Nullable="false"/></Action>
    <Action Name="Reset" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/></Action>
    <Action Name="IssueReceipt" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Temper.App.Receipt" Nullable="false"/></Action>
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
    entity.actions.extend([
        "AttemptCount".into(),
        "MaybeStart".into(),
        "Outcome".into(),
        "Reset".into(),
        "StartWork".into(),
    ]);
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
    assert!(first.source.contains("pub fn maybe_start"));
    assert_eq!(first.source.matches("Result<TypedAction<Task>").count(), 2);
    assert!(first.source.contains("Result<TypedAction<i64>"));
    assert!(
        first
            .source
            .contains("Result<TypedAction<TemperAppOutcome>")
    );
    assert!(
        first
            .source
            .contains("Result<TypedAction<serde_json::Value>")
    );
    assert!(!first.source.contains("TypedAction<TemperAppTaskId>"));
    assert!(first.source.contains("Result<TypedEntity<Task>"));
    assert!(first.source.contains("TEMPER_MODULE_SCHEMA_DIGEST"));
    assert!(first.source.contains("TEMPER_MODULE_USED_SYMBOLS_DIGEST"));
    first.manifest.verify_binding().unwrap();
}

#[test]
fn record_annotations_have_a_canonical_schema_digest() {
    let source = CSDL.replace(
        "</EntityType>",
        "<Annotation Term=\"Example.Metadata\"><Record><PropertyValue Property=\"Zulu\" String=\"last\"/><PropertyValue Property=\"Alpha\" String=\"first\"/></Record></Annotation></EntityType>",
    );
    let mut generated = Vec::new();
    for _ in 0..16 {
        let csdl = parse_csdl(&source).unwrap();
        let sdk = generate_module_sdk(&csdl, "worker", "closure", "closure", "artifact", grant())
            .unwrap();
        generated.push((sdk.source, sdk.manifest.schema_digest));
    }
    assert!(generated.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn cross_entity_action_results_fail_closed() {
    let csdl = parse_csdl(CSDL).unwrap();
    let mut invalid = grant();
    invalid.entities[0].actions.insert("IssueReceipt".into());
    assert!(matches!(
        generate_module_sdk(&csdl, "worker", "closure", "closure", "artifact", invalid),
        Err(ModuleSdkCodegenError::UnsupportedEntityResult {
            action,
            entity_type,
            result_type,
        }) if action == "IssueReceipt"
            && entity_type == "Temper.App.Task"
            && result_type == "Temper.App.Receipt"
    ));
}

#[test]
fn generated_names_preserve_word_boundaries_and_escape_keywords() {
    assert_eq!(rust_field_name("CreatedAt"), "created_at");
    assert_eq!(rust_field_name("created_at"), "created_at");
    assert_eq!(rust_field_name("type"), "type_");
    assert_eq!(rust_field_name("gen"), "gen_");
}

#[test]
fn commit_sequence_property_cannot_collide_with_host_order_helper() {
    let csdl = parse_csdl(&CSDL.replace(
        "<Property Name=\"Status\"",
        "<Property Name=\"CommitSequence\" Type=\"Edm.Int64\" Nullable=\"false\"/><Property Name=\"Status\"",
    ))
    .unwrap();
    let mut invalid = grant();
    invalid.entities[0]
        .query_order_fields
        .insert("CommitSequence".into());
    invalid.entities[0].query_order_by_sequence = true;
    assert!(matches!(
        generate_module_sdk(&csdl, "worker", "closure", "closure", "artifact", invalid),
        Err(ModuleSdkCodegenError::IdentifierCollision(message))
            if message.contains("commit_sequence")
    ));
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
