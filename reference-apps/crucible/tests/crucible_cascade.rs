//! Verification Cascade Tests for the Crucible reference app.
//!
//! Runs the full 3-level `VerificationCascade` against each of Crucible's
//! three IOA specs (`Environment`, `EnvironmentAllowedHost`,
//! `EnvironmentPackage`) and validates that the CSDL and cross-invariant
//! grammars parse cleanly.
//!
//! - L1 Model Check: Stateright exhaustive state exploration
//! - L2 Simulation: Deterministic simulation with fault injection
//! - L3 Property Test: Random action sequences with TLA-style invariants
//!
//! Additionally — as an L4-equivalent sanity pass — the cascade tests
//! validate that every field name referenced in the `[[field_invariant]]`
//! rules and the extended `[[cross_invariant]]` assertions resolves to a
//! property that actually exists in the CSDL. Without this cross-check
//! a typo in a field name would silently degrade a hard constraint into
//! a no-op.

use temper_spec::automaton::parse_automaton;
use temper_spec::cross_invariant::parse_cross_invariants;
use temper_spec::csdl::parse_csdl;
use temper_verify::cascade::{CascadeLevel, VerificationCascade};

const ENVIRONMENT_IOA: &str = include_str!("../specs/environment.ioa.toml");
const ALLOWED_HOST_IOA: &str = include_str!("../specs/environment_allowed_host.ioa.toml");
const PACKAGE_IOA: &str = include_str!("../specs/environment_package.ioa.toml");
const CROSS_INVARIANTS_TOML: &str = include_str!("../specs/cross-invariants.toml");
const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");

fn assert_cascade_passes(name: &str, ioa: &str) {
    let cascade = VerificationCascade::from_ioa(ioa)
        .with_sim_seeds(10)
        .with_prop_test_cases(500);

    let result = cascade.run();

    for level in &result.levels {
        assert!(
            level.passed,
            "{name} cascade level failed: {}",
            level.summary
        );
    }

    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .unwrap()
            .passed,
        "{name} L1 Model Check should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::Simulation)
            .unwrap()
            .passed,
        "{name} L2 Simulation should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::PropertyTest)
            .unwrap()
            .passed,
        "{name} L3 Property Tests should pass"
    );
    assert!(result.all_passed, "{name} cascade should pass all levels");
}

#[test]
fn cascade_environment_all_levels_pass() {
    assert_cascade_passes("Environment", ENVIRONMENT_IOA);
}

#[test]
fn cascade_environment_allowed_host_all_levels_pass() {
    assert_cascade_passes("EnvironmentAllowedHost", ALLOWED_HOST_IOA);
}

#[test]
fn cascade_environment_package_all_levels_pass() {
    assert_cascade_passes("EnvironmentPackage", PACKAGE_IOA);
}

#[test]
fn csdl_parses_and_has_all_entity_types() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    for entity_type in [
        "Environment",
        "EnvironmentAllowedHost",
        "EnvironmentPackage",
    ] {
        assert!(
            schema.entity_type(entity_type).is_some(),
            "CSDL should define {entity_type} entity type"
        );
    }
}

#[test]
fn csdl_defines_archive_environment_action() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    assert!(
        schema.action("ArchiveEnvironment").is_some(),
        "CSDL should define ArchiveEnvironment bound action"
    );
}

#[test]
fn csdl_environment_has_navigation_properties_to_children() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    let env = schema
        .entity_type("Environment")
        .expect("Environment entity type should exist");
    let nav_names: Vec<&str> = env
        .navigation_properties
        .iter()
        .map(|n| n.name.as_str())
        .collect();
    assert!(
        nav_names.contains(&"AllowedHosts"),
        "Environment should have an `AllowedHosts` navigation property, got {nav_names:?}"
    );
    assert!(
        nav_names.contains(&"Packages"),
        "Environment should have a `Packages` navigation property, got {nav_names:?}"
    );
}

#[test]
fn field_invariants_reference_valid_csdl_properties() {
    // Parse the Environment IOA and verify every field name referenced by
    // its `[[field_invariant]]` rules is a real CSDL property. Without this
    // cross-check, a typo in a field name would silently degrade the hard
    // constraint into a no-op.
    let automaton = parse_automaton(ENVIRONMENT_IOA).expect("environment IOA should parse");

    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");
    let env = schema
        .entity_type("Environment")
        .expect("Environment entity type should exist");
    let property_names: Vec<&str> = env.properties.iter().map(|p| p.name.as_str()).collect();

    assert!(
        !automaton.field_invariants.is_empty(),
        "environment.ioa.toml should declare at least one field invariant"
    );

    for invariant in &automaton.field_invariants {
        for field in invariant.referenced_fields() {
            assert!(
                property_names.contains(&field.as_str()),
                "field invariant `{}` references unknown CSDL property: `{field}` (known: {property_names:?})",
                invariant.name,
            );
        }
    }
}

#[test]
fn cross_invariants_reference_valid_csdl_properties() {
    // Parse cross-invariants.toml and verify every field name referenced on
    // the related-entity side resolves to a real CSDL property on the target
    // entity. Uses the extended ADR-0041 grammar:
    // `related(Environment, EnvironmentId).ConfigType not in ["Local"]`.
    use temper_spec::cross_invariant::parse_related_field_assert;

    let cross =
        parse_cross_invariants(CROSS_INVARIANTS_TOML).expect("cross-invariants should parse");

    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    assert!(
        !cross.invariants.is_empty(),
        "cross-invariants.toml should declare at least one invariant"
    );

    for inv in &cross.invariants {
        let parsed = parse_related_field_assert(&inv.assertion).unwrap_or_else(|| {
            panic!(
                "cross-invariant `{}` has unparseable assertion `{}`",
                inv.name, inv.assertion
            )
        });

        let target = schema
            .entity_type(&parsed.target_entity)
            .unwrap_or_else(|| {
                panic!(
                    "cross-invariant `{}` references unknown target entity `{}`",
                    inv.name, parsed.target_entity
                )
            });

        let target_properties: Vec<&str> =
            target.properties.iter().map(|p| p.name.as_str()).collect();
        assert!(
            target_properties.contains(&parsed.field_name.as_str()),
            "cross-invariant `{}` references unknown CSDL property `{}` on `{}` (known: {target_properties:?})",
            inv.name,
            parsed.field_name,
            parsed.target_entity,
        );
    }
}
