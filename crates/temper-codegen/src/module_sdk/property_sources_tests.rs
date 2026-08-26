use std::collections::BTreeSet;

use temper_spec::bundle::IoaSourceInput;
use temper_spec::csdl::parse_csdl;
use temper_wasm_sdk::data::{
    DataOperationKind, EntityDataGrant, ManifestValueSourceV1, ModuleDataGrant,
};

use super::{ModuleSdkCodegenError, generate_module_sdk};

const IOA: &str = r#"[automaton]
name = "Session"
states = ["Unconfigured", "Active"]
initial = "Unconfigured"

[[action]]
name = "Activate"
kind = "input"
from = ["Unconfigured"]
to = "Active"
"#;

fn generate(properties: &str) -> Result<super::GeneratedModuleSdk, ModuleSdkCodegenError> {
    let csdl = parse_csdl(&format!(
        r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EnumType Name="SessionLifecycle"><Member Name="Unconfigured"/><Member Name="Active"/></EnumType><EntityType Name="Session"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/>{properties}</EntityType><EntityContainer Name="Container"><EntitySet Name="Sessions" EntityType="Temper.Test.Session"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#
    ))
    .expect("fixture CSDL parses");
    generate_module_sdk(
        &csdl,
        &[IoaSourceInput {
            entity_type: "Temper.Test.Session".into(),
            source: IOA.into(),
        }],
        "worker",
        "closure",
        "closure",
        "artifact",
        ModuleDataGrant {
            operations: BTreeSet::from([DataOperationKind::EntityGet]),
            entities: vec![EntityDataGrant {
                entity_type: "Temper.Test.Session".into(),
                ..EntityDataGrant::default()
            }],
            ..ModuleDataGrant::default()
        },
    )
}

#[test]
fn lifecycle_and_ordinary_state_sources_are_distinct() {
    let generated = generate(
        r#"<Property Name="State" Type="Edm.String" Nullable="false" DefaultValue="Unconfigured"/><Property Name="RegionState" Type="Edm.String" Nullable="false" DefaultValue="CA"/>"#,
    )
    .expect("one structural lifecycle property should bind");
    let properties = &generated.manifest.entities[0].properties;
    let source = |name: &str| {
        properties
            .iter()
            .find(|property| property.canonical_name == name)
            .map(|property| property.source)
    };
    assert_eq!(source("Id"), Some(ManifestValueSourceV1::EntityId));
    assert_eq!(
        source("State"),
        Some(ManifestValueSourceV1::LifecycleStatus)
    );
    assert_eq!(
        source("RegionState"),
        Some(ManifestValueSourceV1::StoredField)
    );
}

#[test]
fn exact_lifecycle_enum_does_not_require_a_redundant_default() {
    let generated = generate(
        r#"<Property Name="Phase" Type="Temper.Test.SessionLifecycle" Nullable="false"/>"#,
    )
    .expect("an exact lifecycle enum should bind unambiguously");
    assert_eq!(
        generated.manifest.entities[0].properties[1].source,
        ManifestValueSourceV1::LifecycleStatus
    );
}

#[test]
fn ambiguous_lifecycle_candidates_fail_deterministically() {
    let error = generate(
        r#"<Property Name="State" Type="Edm.String" Nullable="false" DefaultValue="Unconfigured"/><Property Name="Status" Type="Edm.String" Nullable="false" DefaultValue="Unconfigured"/>"#,
    )
    .expect_err("two structural candidates must fail closed");
    assert!(matches!(
        error,
        ModuleSdkCodegenError::AmbiguousLifecycleProperty { candidates, .. }
            if candidates == vec!["State", "Status"]
    ));
}

#[test]
fn missing_lifecycle_candidate_fails_before_binding() {
    let error = generate(
        r#"<Property Name="State" Type="Edm.String" Nullable="false" DefaultValue="CA"/>"#,
    )
    .expect_err("ordinary State property must not be treated as lifecycle");
    assert!(matches!(
        error,
        ModuleSdkCodegenError::MissingLifecycleProperty { .. }
    ));
}

#[test]
fn lifecycle_enum_default_must_match_ioa_initial_state() {
    let error = generate(
        r#"<Property Name="Phase" Type="Temper.Test.SessionLifecycle" Nullable="false" DefaultValue="Active"/>"#,
    )
    .expect_err("a contradictory lifecycle default must fail closed");
    assert!(matches!(
        error,
        ModuleSdkCodegenError::LifecycleDefaultMismatch { property, .. } if property == "Phase"
    ));
}
