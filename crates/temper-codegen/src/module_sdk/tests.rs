use super::*;
use temper_spec::{
    IoaSourceInput, ScopedBundleBudgets, ScopedSpecBundle, ScopedSpecBundleInput,
    csdl::{emit_csdl_xml, parse_csdl},
};
use temper_wasm_sdk::data::{DataOperationKind, EntityDataGrant};

mod golden_surfaces;

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
    <EntityType Name="File" HasStream="true"><Key><PropertyRef Name="Id"/></Key>
      <Property Name="Id" Type="Edm.String" Nullable="false"/>
      <Property Name="Path" Type="Edm.String" Nullable="false"/>
    </EntityType>
    <EnumType Name="Outcome"><Member Name="Accepted"/><Member Name="Rejected"/></EnumType>
    <Action Name="StartWork" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Temper.App.Task" Nullable="false"/></Action>
    <Action Name="MaybeStart" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Temper.App.Task" Nullable="true"/></Action>
    <Action Name="AttemptCount" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Edm.Int32" Nullable="false"/></Action>
    <Action Name="Outcome" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Temper.App.Outcome" Nullable="false"/></Action>
    <Action Name="Reset" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/></Action>
    <Action Name="IssueReceipt" IsBound="true"><Parameter Name="binding" Type="Temper.App.Task" Nullable="false"/><ReturnType Type="Temper.App.Receipt" Nullable="false"/></Action>
    <EntityContainer Name="Container">
      <EntitySet Name="Tasks" EntityType="Temper.App.Task"/>
      <EntitySet Name="Files" EntityType="Temper.App.File"/>
    </EntityContainer>
  </Schema></edmx:DataServices>
</edmx:Edmx>"#;

const OVERLOADED_ACTION_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices><Schema Namespace="TemperPaw.ArcAgi2" xmlns="http://docs.oasis-open.org/odata/ns/edm">
    <EntityType Name="ArcTask"><Key><PropertyRef Name="Id"/></Key>
      <Property Name="Id" Type="Edm.String" Nullable="false"/>
    </EntityType>
    <EntityType Name="ArcInferenceCandidate"><Key><PropertyRef Name="Id"/></Key>
      <Property Name="Id" Type="Edm.String" Nullable="false"/>
    </EntityType>
    <Action Name="Configure" IsBound="true">
      <Parameter Name="binding" Type="TemperPaw.ArcAgi2.ArcTask" Nullable="false"/>
      <Parameter Name="TaskPrompt" Type="Edm.String" Nullable="false"/>
      <ReturnType Type="Edm.String" Nullable="false"/>
    </Action>
    <Action Name="Configure" IsBound="true">
      <Parameter Name="binding" Type="TemperPaw.ArcAgi2.ArcInferenceCandidate" Nullable="false"/>
      <Parameter Name="AttemptBudget" Type="Edm.Int64" Nullable="false"/>
      <ReturnType Type="Edm.Boolean" Nullable="false"/>
    </Action>
    <Action Name="RecordVerified" IsBound="true">
      <Parameter Name="binding" Type="TemperPaw.ArcAgi2.ArcTask" Nullable="false"/>
      <Parameter Name="Verifier" Type="Edm.String" Nullable="false"/>
    </Action>
    <Action Name="RecordVerified" IsBound="true">
      <Parameter Name="binding" Type="TemperPaw.ArcAgi2.ArcInferenceCandidate" Nullable="false"/>
      <Parameter Name="Confidence" Type="Edm.Decimal" Nullable="false"/>
    </Action>
    <Action Name="RecordRejected" IsBound="true">
      <Parameter Name="binding" Type="TemperPaw.ArcAgi2.ArcTask" Nullable="false"/>
      <Parameter Name="Verifier" Type="Edm.String" Nullable="false"/>
    </Action>
    <Action Name="RecordRejected" IsBound="true">
      <Parameter Name="binding" Type="TemperPaw.ArcAgi2.ArcInferenceCandidate" Nullable="false"/>
      <Parameter Name="Reason" Type="Edm.String" Nullable="false"/>
    </Action>
    <Action Name="Configure" IsBound="false">
      <Parameter Name="GlobalMode" Type="Edm.String" Nullable="false"/>
    </Action>
    <EntityContainer Name="Container">
      <EntitySet Name="ArcTasks" EntityType="TemperPaw.ArcAgi2.ArcTask"/>
      <EntitySet Name="ArcInferenceCandidates" EntityType="TemperPaw.ArcAgi2.ArcInferenceCandidate"/>
    </EntityContainer>
  </Schema></edmx:DataServices>
</edmx:Edmx>"#;

const ARC_TASK_IOA: &str = r#"
[automaton]
name = "ArcTask"
states = ["Open"]
initial = "Open"

[[state]]
name = "lifecycle"
type = "status"
initial = "Open"
"#;

const ARC_CANDIDATE_IOA: &str = r#"
[automaton]
name = "ArcInferenceCandidate"
states = ["Open"]
initial = "Open"

[[state]]
name = "lifecycle"
type = "status"
initial = "Open"
"#;

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

fn arc_candidate_grant() -> ModuleDataGrant {
    ModuleDataGrant {
        operations: [DataOperationKind::ActionInvoke].into_iter().collect(),
        entities: vec![EntityDataGrant {
            entity_type: "TemperPaw.ArcAgi2.ArcInferenceCandidate".into(),
            actions: [
                "Configure".into(),
                "RecordRejected".into(),
                "RecordVerified".into(),
            ]
            .into_iter()
            .collect(),
            ..EntityDataGrant::default()
        }],
        ..ModuleDataGrant::default()
    }
}

fn arc_task_grant() -> ModuleDataGrant {
    ModuleDataGrant {
        operations: [DataOperationKind::ActionInvoke].into_iter().collect(),
        entities: vec![EntityDataGrant {
            entity_type: "TemperPaw.ArcAgi2.ArcTask".into(),
            actions: ["Configure".into()].into_iter().collect(),
            ..EntityDataGrant::default()
        }],
        ..ModuleDataGrant::default()
    }
}

fn canonical_arc_csdl(source: &str) -> CsdlDocument {
    let bundle = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: "arc-overload-regression".into(),
        predecessor_digest: None,
        csdl_xml: source.into(),
        ioa_sources: vec![
            IoaSourceInput {
                entity_type: "TemperPaw.ArcAgi2.ArcTask".into(),
                source: ARC_TASK_IOA.into(),
            },
            IoaSourceInput {
                entity_type: "TemperPaw.ArcAgi2.ArcInferenceCandidate".into(),
                source: ARC_CANDIDATE_IOA.into(),
            },
        ],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: None,
        budgets: ScopedBundleBudgets::default(),
    })
    .expect("ARC-shaped fixture should canonicalize as a verified bundle");
    parse_csdl(bundle.canonical_csdl()).unwrap()
}

#[test]
fn generation_resolves_arc_shaped_actions_by_exact_binding_entity() {
    let csdl = parse_csdl(OVERLOADED_ACTION_CSDL).unwrap();
    let generated = generate_module_sdk(
        &csdl,
        "arc-inference",
        "closure",
        "lock",
        "artifact",
        arc_candidate_grant(),
    )
    .expect("the candidate-bound overloads should satisfy the candidate grant");

    let candidate = &generated.manifest.entities[0];
    let configure = candidate
        .actions
        .iter()
        .find(|action| action.canonical_name == "Configure")
        .unwrap();
    assert_eq!(configure.parameters[0].canonical_name, "AttemptBudget");
    assert_eq!(configure.parameters[0].type_name, "Edm.Int64");
    assert_eq!(configure.result_type.as_deref(), Some("Edm.Boolean"));
    assert!(generated.source.contains("pub fn configure"));
    assert!(generated.source.contains("pub attempt_budget: i64"));
    assert!(!generated.source.contains("pub task_prompt: String"));

    let task_generated = generate_module_sdk(
        &csdl,
        "arc-task",
        "closure",
        "lock",
        "artifact",
        arc_task_grant(),
    )
    .expect("the task-bound overload should satisfy the task grant");
    let task_configure = &task_generated.manifest.entities[0].actions[0];
    assert_eq!(task_configure.parameters[0].canonical_name, "TaskPrompt");
    assert_eq!(task_configure.parameters[0].type_name, "Edm.String");
    assert_eq!(task_configure.result_type.as_deref(), Some("Edm.String"));
    assert!(task_generated.source.contains("pub task_prompt: String"));
    assert!(!task_generated.source.contains("pub attempt_budget: i64"));
}

#[test]
fn missing_exact_bound_action_ignores_other_bindings_and_unbound_overloads() {
    let mut csdl = parse_csdl(OVERLOADED_ACTION_CSDL).unwrap();
    csdl.schemas[0].actions.retain(|action| {
        action.name != "Configure"
            || action.binding_type() != Some("TemperPaw.ArcAgi2.ArcInferenceCandidate")
    });

    assert!(matches!(
        generate_module_sdk(
            &csdl,
            "arc-inference",
            "closure",
            "lock",
            "artifact",
            arc_candidate_grant(),
        ),
        Err(ModuleSdkCodegenError::MissingSymbol { entity_type, symbol })
            if entity_type == "TemperPaw.ArcAgi2.ArcInferenceCandidate"
                && symbol == "Configure"
    ));
}

#[test]
fn ambiguous_exact_bound_actions_fail_closed() {
    let mut csdl = parse_csdl(OVERLOADED_ACTION_CSDL).unwrap();
    let mut duplicate = csdl.schemas[0]
        .bound_actions("Configure", "TemperPaw.ArcAgi2.ArcInferenceCandidate")
        .into_iter()
        .next()
        .unwrap()
        .clone();
    duplicate.parameters[1].name = "CandidateMode".into();
    duplicate.parameters[1].type_name = "Edm.String".into();
    csdl.schemas[0].actions.push(duplicate);

    assert!(matches!(
        generate_module_sdk(
            &csdl,
            "arc-inference",
            "closure",
            "lock",
            "artifact",
            arc_candidate_grant(),
        ),
        Err(ModuleSdkCodegenError::AmbiguousBoundAction {
            entity_type,
            action,
            matches: 2,
        }) if entity_type == "TemperPaw.ArcAgi2.ArcInferenceCandidate"
            && action == "Configure"
    ));
}

#[test]
fn canonical_action_order_keeps_generated_sdk_and_binding_identical() {
    let original = canonical_arc_csdl(OVERLOADED_ACTION_CSDL);
    let mut reordered = parse_csdl(OVERLOADED_ACTION_CSDL).unwrap();
    reordered.schemas[0].actions.reverse();
    let reordered_source = emit_csdl_xml(&reordered);
    let reordered = canonical_arc_csdl(&reordered_source);
    assert_eq!(emit_csdl_xml(&original), emit_csdl_xml(&reordered));

    let first = generate_module_sdk(
        &original,
        "arc-inference",
        "closure",
        "lock",
        "",
        arc_candidate_grant(),
    )
    .unwrap();
    let second = generate_module_sdk(
        &reordered,
        "arc-inference",
        "closure",
        "lock",
        "",
        arc_candidate_grant(),
    )
    .unwrap();
    assert_eq!(first.source, second.source);
    assert_eq!(first.manifest, second.manifest);
    assert_eq!(first.manifest.grant_digest, second.manifest.grant_digest);
    assert_eq!(
        first.manifest.binding_digest().unwrap(),
        second.manifest.binding_digest().unwrap()
    );

    let first = package_generated_module_sdk(b"\0asm\x01\0\0\0", first).unwrap();
    let second = package_generated_module_sdk(b"\0asm\x01\0\0\0", second).unwrap();
    assert_eq!(first.wasm, second.wasm);
    assert_eq!(first.manifest, second.manifest);
    assert_eq!(
        first.manifest.artifact_digest,
        second.manifest.artifact_digest
    );
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
    assert!(!first.source.contains("TaskCreate"));
    assert!(!first.source.contains("TaskPatch"));
    assert!(!first.source.contains("TaskFilter"));
    assert!(!first.source.contains("TaskOrder"));
    assert!(!first.source.contains("pub fn query("));
    assert!(!first.source.contains("pub fn create("));
    assert!(!first.source.contains("pub fn patch("));
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
