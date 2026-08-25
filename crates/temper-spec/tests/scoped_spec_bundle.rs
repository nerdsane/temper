use temper_spec::{
    BundleErrorCode, IoaSourceInput, MigrationArtifactInput, PolicyArtifactInput,
    ScopedBundleBudgets, ScopedSpecBundle, ScopedSpecBundleInput, WasmArtifactInput,
    parse_automaton, parse_csdl,
};

const ALPHA_IOA: &str = r#"
[automaton]
name = "Alpha"
states = ["Draft", "Ready"]
initial = "Draft"

[[state]]
name = "lifecycle"
type = "status"
initial = "Draft"

[[action]]
name = "Ready"
kind = "input"
from = ["Draft"]
to = "Ready"
"#;

const BETA_IOA: &str = r#"
[automaton]
name = "Beta"
states = ["Open"]
initial = "Open"

[[state]]
name = "lifecycle"
type = "status"
initial = "Open"
"#;

const ACTION_TRIGGER_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Alpha"
states = ["Created", "Running", "Failed"]
initial = "Created"

[[state]]
name = "lifecycle"
type = "status"
initial = "Created"

[[action]]
name = "Start"
kind = "input"
from = ["Created"]
to = "Running"
guard = [{ type = "state_in", values = ["Created"] }]

[[action.triggers]]
name = "run_worker"
kind = "wasm"
module = "worker"
on_failure = "Fail"

[action.triggers.config]
temper_api_url = "{secret:temper_api_url}"

[[action]]
name = "Fail"
kind = "input"
from = ["Created", "Running"]
to = "Failed"
params = ["error_message"]
effect = [
  { type = "trigger", name = "record_failure" },
  { type = "trigger", name = "notify_operator" },
]

[[action.triggers]]
name = "record_failure"
kind = "wasm"
module = "failure_recorder"

[[action.triggers]]
name = "notify_operator"
kind = "wasm"
module = "operator_notifier"

[[state_timeout]]
state = "Created"
after_seconds = 60
on_timeout = "Fail"
params = { error_message = "start never arrived" }
"#;

const ORDERED_CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Alpha">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
        <Property Name="Label" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Beta">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Service">
        <EntitySet Name="Alphas" EntityType="Example.Alpha"/>
        <EntitySet Name="Betas" EntityType="Example.Beta"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const REORDERED_CSDL: &str = r#"
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0">
 <edmx:DataServices>
  <Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="Example">
   <EntityContainer Name="Service">
    <EntitySet EntityType="Example.Beta" Name="Betas" />
    <EntitySet EntityType="Example.Alpha" Name="Alphas" />
   </EntityContainer>
   <EntityType Name="Beta">
    <Property Nullable="false" Type="Edm.Guid" Name="Id" />
    <Key><PropertyRef Name="Id" /></Key>
   </EntityType>
   <EntityType Name="Alpha">
    <Property Type="Edm.String" Name="Label" />
    <Property Type="Edm.Guid" Nullable="false" Name="Id" />
    <Key><PropertyRef Name="Id" /></Key>
   </EntityType>
  </Schema>
 </edmx:DataServices>
</edmx:Edmx>
"#;

fn input(csdl_xml: &str, ioa_sources: Vec<(&str, &str)>) -> ScopedSpecBundleInput {
    ScopedSpecBundleInput {
        scope_id: "task-42".into(),
        predecessor_digest: None,
        csdl_xml: csdl_xml.into(),
        ioa_sources: ioa_sources
            .into_iter()
            .map(|(entity_type, source)| IoaSourceInput {
                entity_type: entity_type.into(),
                source: source.into(),
            })
            .collect(),
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: None,
        budgets: ScopedBundleBudgets::default(),
    }
}

#[test]
fn canonical_bundle_identity_ignores_formatting_and_input_order() {
    let first = ScopedSpecBundle::compile(input(
        ORDERED_CSDL,
        vec![("Example.Alpha", ALPHA_IOA), ("Example.Beta", BETA_IOA)],
    ))
    .expect("first bundle should compile");
    let second = ScopedSpecBundle::compile(input(
        REORDERED_CSDL,
        vec![
            ("Example.Beta", &format!("\n{BETA_IOA}\n")),
            ("Example.Alpha", &ALPHA_IOA.replace("\n", "\r\n")),
        ],
    ))
    .expect("reordered bundle should compile");

    assert_eq!(first.digest(), second.digest());
    assert_eq!(first.canonical_csdl(), second.canonical_csdl());
    assert_eq!(first.ioa_specs(), second.ioa_specs());
    assert_eq!(
        first.digest(),
        "sha256:1b6d8593187902e64156ff79c09b5738bbaae2f6170558a24b63d7106864fadb"
    );
}

#[test]
fn canonical_sources_round_trip_without_changing_identity() {
    let compiled = ScopedSpecBundle::compile(input(
        REORDERED_CSDL,
        vec![("Example.Beta", BETA_IOA), ("Example.Alpha", ALPHA_IOA)],
    ))
    .expect("bundle should compile");

    parse_csdl(compiled.canonical_csdl()).expect("canonical CSDL should parse");
    for spec in compiled.ioa_specs() {
        let automaton = parse_automaton(&spec.canonical_source)
            .expect("canonical IOA source should parse through the typed parser");
        assert_eq!(
            automaton.automaton.name,
            spec.entity_type.rsplit('.').next().unwrap()
        );
    }

    let recompiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: compiled.scope_id().into(),
        predecessor_digest: compiled.predecessor_digest().map(str::to_string),
        csdl_xml: compiled.canonical_csdl().into(),
        ioa_sources: compiled
            .ioa_specs()
            .iter()
            .map(|spec| IoaSourceInput {
                entity_type: spec.entity_type.clone(),
                source: spec.canonical_source.clone(),
            })
            .collect(),
        cedar_policies: compiled.cedar_policies().to_vec(),
        wasm_modules: compiled.wasm_modules().to_vec(),
        migration: compiled.migration().cloned(),
        budgets: compiled.budgets().clone(),
    })
    .expect("canonical bundle should recompile");

    assert_eq!(compiled, recompiled);
}

#[test]
fn canonical_source_preserves_actions_after_nested_trigger_config() {
    let compiled = ScopedSpecBundle::compile(input(
        ORDERED_CSDL,
        vec![("Example.Alpha", ACTION_TRIGGER_TIMEOUT_IOA)],
    ))
    .expect("nested trigger configuration must not hide later timeout actions");

    let automaton = parse_automaton(&compiled.ioa_specs()[0].canonical_source)
        .expect("canonical IOA source should preserve the timeout action");
    let start = automaton
        .actions
        .iter()
        .find(|action| action.name == "Start")
        .expect("Start action should survive canonicalization");
    assert_eq!(
        start.guard.len(),
        1,
        "Start guard should survive canonicalization"
    );

    let fail = automaton
        .actions
        .iter()
        .find(|action| action.name == "Fail")
        .expect("Fail action should survive canonicalization");
    assert_eq!(
        fail.effect.len(),
        2,
        "Fail effects should survive canonicalization"
    );
}

#[test]
fn bundle_identity_is_bound_to_scope_predecessor_and_semantics() {
    let baseline = ScopedSpecBundle::compile(input(
        ORDERED_CSDL,
        vec![("Example.Alpha", ALPHA_IOA), ("Example.Beta", BETA_IOA)],
    ))
    .unwrap();

    let mut different_scope = input(
        ORDERED_CSDL,
        vec![("Example.Alpha", ALPHA_IOA), ("Example.Beta", BETA_IOA)],
    );
    different_scope.scope_id = "task-43".into();
    assert_ne!(
        baseline.digest(),
        ScopedSpecBundle::compile(different_scope).unwrap().digest()
    );

    let mut with_predecessor = input(
        ORDERED_CSDL,
        vec![("Example.Alpha", ALPHA_IOA), ("Example.Beta", BETA_IOA)],
    );
    with_predecessor.predecessor_digest = Some(format!("sha256:{}", "a".repeat(64)));
    assert_ne!(
        baseline.digest(),
        ScopedSpecBundle::compile(with_predecessor)
            .unwrap()
            .digest()
    );

    let changed = ALPHA_IOA.replace("to = \"Ready\"", "to = \"Draft\"");
    let semantic_change = ScopedSpecBundle::compile(input(
        ORDERED_CSDL,
        vec![("Example.Alpha", &changed), ("Example.Beta", BETA_IOA)],
    ))
    .unwrap();
    assert_ne!(baseline.digest(), semantic_change.digest());
}

#[test]
fn named_artifacts_are_ordered_and_line_endings_are_canonical() {
    let digest_a = format!("sha256:{}", "a".repeat(64));
    let digest_b = format!("sha256:{}", "b".repeat(64));
    let mut first = input(ORDERED_CSDL, vec![("Example.Alpha", ALPHA_IOA)]);
    first.cedar_policies = vec![
        PolicyArtifactInput {
            name: "z-policy".into(),
            source: "permit(\r\nprincipal,\r\naction,\r\nresource);\r\n".into(),
        },
        PolicyArtifactInput {
            name: "a-policy".into(),
            source: "forbid(principal, action, resource);\n".into(),
        },
    ];
    first.wasm_modules = vec![
        WasmArtifactInput {
            name: "z-module".into(),
            artifact_digest: digest_b.clone(),
        },
        WasmArtifactInput {
            name: "a-module".into(),
            artifact_digest: digest_a.clone(),
        },
    ];
    first.migration = Some(MigrationArtifactInput {
        name: "migrate".into(),
        artifact_digest: digest_a.clone(),
        abi_version: "temper-schema-migration/v1".into(),
    });

    let mut second = input(ORDERED_CSDL, vec![("Example.Alpha", ALPHA_IOA)]);
    second.cedar_policies = vec![
        PolicyArtifactInput {
            name: "a-policy".into(),
            source: "forbid(principal, action, resource);\n".into(),
        },
        PolicyArtifactInput {
            name: "z-policy".into(),
            source: "permit(\nprincipal,\naction,\nresource);\n".into(),
        },
    ];
    second.wasm_modules = vec![
        WasmArtifactInput {
            name: "a-module".into(),
            artifact_digest: digest_a,
        },
        WasmArtifactInput {
            name: "z-module".into(),
            artifact_digest: digest_b,
        },
    ];
    second.migration = first.migration.clone();

    assert_eq!(
        ScopedSpecBundle::compile(first).unwrap(),
        ScopedSpecBundle::compile(second).unwrap()
    );
}

#[test]
fn artifact_duplicates_and_zero_budgets_fail_closed() {
    let mut duplicate = input(ORDERED_CSDL, vec![("Example.Alpha", ALPHA_IOA)]);
    duplicate.cedar_policies = vec![
        PolicyArtifactInput {
            name: "policy".into(),
            source: "permit(principal, action, resource);".into(),
        },
        PolicyArtifactInput {
            name: "policy".into(),
            source: "forbid(principal, action, resource);".into(),
        },
    ];
    assert_eq!(
        ScopedSpecBundle::compile(duplicate).unwrap_err().code(),
        BundleErrorCode::DuplicateSymbol
    );

    let mut zero_budget = input(ORDERED_CSDL, vec![("Example.Alpha", ALPHA_IOA)]);
    zero_budget.budgets.migration_entities_per_batch = 0;
    assert_eq!(
        ScopedSpecBundle::compile(zero_budget).unwrap_err().code(),
        BundleErrorCode::BudgetExceeded
    );
}

#[test]
fn duplicate_ioa_entity_is_rejected_without_last_writer_wins() {
    let error = ScopedSpecBundle::compile(input(
        ORDERED_CSDL,
        vec![("Example.Alpha", ALPHA_IOA), ("Example.Alpha", ALPHA_IOA)],
    ))
    .unwrap_err();

    assert_eq!(error.code(), BundleErrorCode::DuplicateSymbol);
    assert!(error.to_string().contains("Example.Alpha"));
}

#[test]
fn duplicate_csdl_entity_is_rejected() {
    let csdl = ORDERED_CSDL.replace(
        "      <EntityType Name=\"Beta\">",
        "      <EntityType Name=\"Alpha\"><Property Name=\"Other\" Type=\"Edm.String\"/></EntityType>\n      <EntityType Name=\"Beta\">",
    );
    let error = ScopedSpecBundle::compile(input(
        &csdl,
        vec![("Example.Alpha", ALPHA_IOA), ("Example.Beta", BETA_IOA)],
    ))
    .unwrap_err();

    assert_eq!(error.code(), BundleErrorCode::DuplicateSymbol);
    assert!(error.to_string().contains("Example.Alpha"));
}

#[test]
fn ioa_key_must_match_the_typed_automaton_name() {
    let error =
        ScopedSpecBundle::compile(input(ORDERED_CSDL, vec![("Example.NotAlpha", ALPHA_IOA)]))
            .unwrap_err();

    assert_eq!(error.code(), BundleErrorCode::EntityNameMismatch);
    assert!(error.to_string().contains("NotAlpha"));
    assert!(error.to_string().contains("Alpha"));
}

#[test]
fn invalid_inputs_have_stable_error_codes() {
    let mut empty_scope = input(ORDERED_CSDL, vec![("Example.Alpha", ALPHA_IOA)]);
    empty_scope.scope_id = "  ".into();
    assert_eq!(
        ScopedSpecBundle::compile(empty_scope).unwrap_err().code(),
        BundleErrorCode::InvalidScope
    );

    let invalid_ioa = input(ORDERED_CSDL, vec![("Example.Alpha", "not = [valid")]);
    assert_eq!(
        ScopedSpecBundle::compile(invalid_ioa).unwrap_err().code(),
        BundleErrorCode::InvalidIoa
    );

    let invalid_csdl = input("<broken", vec![("Example.Alpha", ALPHA_IOA)]);
    assert_eq!(
        ScopedSpecBundle::compile(invalid_csdl).unwrap_err().code(),
        BundleErrorCode::InvalidCsdl
    );

    let no_ioa = input(ORDERED_CSDL, vec![]);
    assert_eq!(
        ScopedSpecBundle::compile(no_ioa).unwrap_err().code(),
        BundleErrorCode::InvalidIoa
    );

    let mut invalid_predecessor = input(ORDERED_CSDL, vec![("Example.Alpha", ALPHA_IOA)]);
    invalid_predecessor.predecessor_digest = Some("SHA256:not-canonical".into());
    assert_eq!(
        ScopedSpecBundle::compile(invalid_predecessor)
            .unwrap_err()
            .code(),
        BundleErrorCode::InvalidPredecessor
    );

    let huge_name = format!("Example.{}", "é".repeat(8_000));
    let bounded_error =
        ScopedSpecBundle::compile(input(ORDERED_CSDL, vec![(huge_name.as_str(), ALPHA_IOA)]))
            .unwrap_err()
            .to_string();
    assert!(bounded_error.len() <= 1_100);
    assert!(bounded_error.is_char_boundary(bounded_error.len()));
}

#[test]
fn bundle_rejects_typed_references_outside_its_ioa_set() {
    let referencing_ioa = r#"
[automaton]
name = "Alpha"
states = ["Draft"]
initial = "Draft"

[[state]]
name = "beta_id"
type = "ref"
entity_type = "Beta"
initial = ""
"#;

    let error = ScopedSpecBundle::compile(input(
        ORDERED_CSDL,
        vec![("Example.Alpha", referencing_ioa)],
    ))
    .unwrap_err();

    assert_eq!(error.code(), BundleErrorCode::InvalidBundle);
    assert!(error.to_string().contains("reference_target_missing"));
    assert!(error.to_string().contains("Beta"));
}

#[test]
fn bundle_rejects_csdl_reference_contract_contradictions() {
    let referencing_ioa = r#"
[automaton]
name = "Alpha"
states = ["Draft"]
initial = "Draft"

[[state]]
name = "beta_id"
type = "ref"
entity_type = "Beta"
initial = ""
"#;
    let contradictory_csdl = ORDERED_CSDL.replace(
        "        <Property Name=\"Label\" Type=\"Edm.String\"/>",
        "        <Property Name=\"Label\" Type=\"Edm.String\"/>\n        <Property Name=\"beta_id\" Type=\"Edm.Guid\"/>\n        <NavigationProperty Name=\"Beta\" Type=\"Example.Alpha\"><ReferentialConstraint Property=\"beta_id\" ReferencedProperty=\"Id\"/></NavigationProperty>",
    );

    let error = ScopedSpecBundle::compile(input(
        &contradictory_csdl,
        vec![
            ("Example.Alpha", referencing_ioa),
            ("Example.Beta", BETA_IOA),
        ],
    ))
    .unwrap_err();

    assert_eq!(error.code(), BundleErrorCode::InvalidBundle);
    assert!(
        error
            .to_string()
            .contains("csdl_reference_contract_mismatch")
    );
}
