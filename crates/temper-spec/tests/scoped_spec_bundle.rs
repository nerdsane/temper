use temper_spec::{
    BundleErrorCode, IoaSourceInput, MigrationArtifactInput, PolicyArtifactInput,
    ScopedBundleBudgets, ScopedSpecBundle, ScopedSpecBundleInput, WasmArtifactInput,
    parse_automaton, parse_csdl, scoped_module_data_closure_digest,
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

const TYPED_FAILURE_ROUTE_IOA: &str = r#"
[automaton]
name = "Alpha"
states = ["Created", "Running", "RetryScheduled"]
initial = "Created"

[[action]]
name = "Start"
kind = "input"
from = ["Created"]
to = "Running"

[[action.triggers]]
name = "run_worker"
kind = "wasm"
module = "worker"

[[action.triggers.failure_routes]]
category = "transient"
action = "RecordTransientFailureV1"

[[action]]
name = "RecordTransientFailureV1"
kind = "input"
from = ["Running"]
to = "RetryScheduled"
params = [{ name = "failure", type = "failure_v1" }]
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
      <Action Name="Ready" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/></Action>
      <Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/></Action>
      <Action Name="Fail" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/><Parameter Name="error_message" Type="Edm.String" Nullable="false"/></Action>
      <Action Name="RecordTransientFailureV1" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/><Parameter Name="failure" Type="failure_v1" Nullable="false"/></Action>
      <Action Name="Adjust" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/><Parameter Name="delta" Type="Edm.Int64"/></Action>
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
   <Action Name="Ready" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/></Action>
   <Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/></Action>
   <Action Name="Fail" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/><Parameter Name="error_message" Type="Edm.String" Nullable="false"/></Action>
   <Action Name="RecordTransientFailureV1" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/><Parameter Name="failure" Type="failure_v1" Nullable="false"/></Action>
   <Action Name="Adjust" IsBound="true"><Parameter Name="bindingParameter" Type="Example.Alpha" Nullable="false"/><Parameter Name="delta" Type="Edm.Int64"/></Action>
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

    assert_eq!(
        first.canonical_csdl(),
        second.canonical_csdl(),
        "canonical CSDL must ignore source ordering"
    );
    assert_eq!(first.digest(), second.digest());
    assert_eq!(first.ioa_specs(), second.ioa_specs());
    assert_eq!(
        first.digest(),
        "sha256:0c2453c7521f327e84be1e21a9afdc8cce3836bf38aeceb166cd876f780d9fca"
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
fn canonical_bundle_preserves_typed_failure_callback_parameters() {
    let compiled = ScopedSpecBundle::compile(input(
        ORDERED_CSDL,
        vec![("Example.Alpha", TYPED_FAILURE_ROUTE_IOA)],
    ))
    .expect("typed failure callback bundle should compile");

    let canonical = &compiled.ioa_specs()[0].canonical_source;
    assert!(canonical.contains("[[action.params]]"));
    let automaton = parse_automaton(canonical).expect("canonical IOA should reparse");
    let callback = automaton
        .actions
        .iter()
        .find(|action| action.name == "RecordTransientFailureV1")
        .expect("callback action should retain its identity");
    assert_eq!(callback.params.len(), 1);
    assert_eq!(callback.params[0].name(), "failure");
    assert_eq!(callback.params[0].param_type(), "failure_v1");

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
    .expect("canonical typed failure callback bundle should recompile");
    assert_eq!(compiled, recompiled);
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
            data_binding_digest: None,
        },
        WasmArtifactInput {
            name: "a-module".into(),
            artifact_digest: digest_a.clone(),
            data_binding_digest: None,
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
            data_binding_digest: None,
        },
        WasmArtifactInput {
            name: "z-module".into(),
            artifact_digest: digest_b,
            data_binding_digest: None,
        },
    ];
    second.migration = first.migration.clone();

    assert_eq!(
        ScopedSpecBundle::compile(first).unwrap(),
        ScopedSpecBundle::compile(second).unwrap()
    );
}

#[test]
fn module_data_binding_digest_is_part_of_bundle_identity() {
    let artifact_digest = format!("sha256:{}", "a".repeat(64));
    let mut unbound = input(ORDERED_CSDL, vec![("Example.Alpha", ALPHA_IOA)]);
    unbound.wasm_modules = vec![WasmArtifactInput {
        name: "worker".into(),
        artifact_digest: artifact_digest.clone(),
        data_binding_digest: None,
    }];
    let mut bound = unbound.clone();
    bound.wasm_modules[0].data_binding_digest = Some(format!("sha256:{}", "b".repeat(64)));

    assert_ne!(
        ScopedSpecBundle::compile(unbound).unwrap().digest(),
        ScopedSpecBundle::compile(bound).unwrap().digest(),
        "typed-data authority must be immutable bundle content"
    );
}

#[test]
fn module_data_closure_digest_is_canonical_and_excludes_artifacts() {
    let ordered = scoped_module_data_closure_digest(
        ORDERED_CSDL,
        vec![
            IoaSourceInput {
                entity_type: "Example.Alpha".into(),
                source: ALPHA_IOA.into(),
            },
            IoaSourceInput {
                entity_type: "Example.Beta".into(),
                source: BETA_IOA.into(),
            },
        ],
    )
    .unwrap();
    let reordered = scoped_module_data_closure_digest(
        REORDERED_CSDL,
        vec![
            IoaSourceInput {
                entity_type: "Example.Beta".into(),
                source: BETA_IOA.into(),
            },
            IoaSourceInput {
                entity_type: "Example.Alpha".into(),
                source: ALPHA_IOA.into(),
            },
        ],
    )
    .unwrap();

    assert_eq!(ordered, reordered);
    assert!(ordered.starts_with("sha256:"));
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

#[test]
fn scoped_bundle_admission_runs_nullable_consumer_lint() {
    let ioa = r#"
[automaton]
name = "Alpha"
states = ["Draft"]
initial = "Draft"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[action]]
name = "Adjust"
from = ["Draft"]
params = [{ name = "delta", type = "Edm.Int64", nullable = true }]
effect = [{ type = "increment", var = "count", amount = "delta" }]
"#;
    let error =
        ScopedSpecBundle::compile(input(ORDERED_CSDL, vec![("Example.Alpha", ioa)])).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("nullable_action_parameter_consumed")
    );
}

#[test]
fn scoped_bundle_admission_runs_ioa_csdl_action_contract_lint() {
    let ioa = r#"
[automaton]
name = "Alpha"
states = ["Draft", "Ready"]
initial = "Draft"

[[action]]
name = "Ready"
from = ["Draft"]
to = "Ready"
params = [{ name = "Code", type = "Edm.String" }]
"#;
    let csdl = ORDERED_CSDL.replace(
        "      <Action Name=\"Ready\" IsBound=\"true\"><Parameter Name=\"bindingParameter\" Type=\"Example.Alpha\" Nullable=\"false\"/></Action>",
        "      <Action Name=\"Ready\" IsBound=\"true\"><Parameter Name=\"bindingParameter\" Type=\"Example.Alpha\" Nullable=\"false\"/><Parameter Name=\"Code\" Type=\"Edm.String\"/></Action>",
    );
    let error = ScopedSpecBundle::compile(input(&csdl, vec![("Example.Alpha", ioa)])).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("csdl_action_parameter_requiredness_mismatch")
    );
}
