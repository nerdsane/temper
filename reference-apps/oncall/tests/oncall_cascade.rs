//! Verification Cascade Tests for on-call entities.
//!
//! Runs the full 4-level VerificationCascade on each entity spec:
//! - Level 0: SMT symbolic verification (guard satisfiability, invariant induction)
//! - Level 1: Stateright exhaustive model checking
//! - Level 2: Deterministic simulation with fault injection
//! - Level 3: Property-based testing with random action sequences

use temper_verify::cascade::{CascadeLevel, VerificationCascade};

const PAGE_IOA: &str = include_str!("../specs/page.ioa.toml");
const ESCALATION_POLICY_IOA: &str = include_str!("../specs/escalation_policy.ioa.toml");
const REMEDIATION_IOA: &str = include_str!("../specs/remediation.ioa.toml");
const POSTMORTEM_IOA: &str = include_str!("../specs/postmortem.ioa.toml");

#[test]
fn cascade_page_all_levels_pass() {
    let cascade = VerificationCascade::from_ioa(PAGE_IOA)
        .with_sim_seeds(10)
        .with_prop_test_cases(1000);

    let result = cascade.run();

    for level in &result.levels {
        assert!(level.passed, "Page cascade level failed: {}", level.summary);
    }

    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .unwrap()
            .passed,
        "L1 Model Check should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::Simulation)
            .unwrap()
            .passed,
        "L2 Simulation should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::PropertyTest)
            .unwrap()
            .passed,
        "L3 Property Tests should pass"
    );
    assert!(result.all_passed, "Page cascade should pass all levels");
}

#[test]
fn cascade_escalation_policy_all_levels_pass() {
    let cascade = VerificationCascade::from_ioa(ESCALATION_POLICY_IOA)
        .with_sim_seeds(10)
        .with_prop_test_cases(1000);

    let result = cascade.run();

    for level in &result.levels {
        assert!(
            level.passed,
            "EscalationPolicy cascade level failed: {}",
            level.summary
        );
    }

    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .unwrap()
            .passed,
        "L1 Model Check should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::Simulation)
            .unwrap()
            .passed,
        "L2 Simulation should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::PropertyTest)
            .unwrap()
            .passed,
        "L3 Property Tests should pass"
    );
    assert!(
        result.all_passed,
        "EscalationPolicy cascade should pass all levels"
    );
}

#[test]
fn cascade_remediation_all_levels_pass() {
    let cascade = VerificationCascade::from_ioa(REMEDIATION_IOA)
        .with_sim_seeds(10)
        .with_prop_test_cases(1000);

    let result = cascade.run();

    for level in &result.levels {
        assert!(
            level.passed,
            "Remediation cascade level failed: {}",
            level.summary
        );
    }

    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .unwrap()
            .passed,
        "L1 Model Check should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::Simulation)
            .unwrap()
            .passed,
        "L2 Simulation should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::PropertyTest)
            .unwrap()
            .passed,
        "L3 Property Tests should pass"
    );
    assert!(
        result.all_passed,
        "Remediation cascade should pass all levels"
    );
}

#[test]
fn cascade_postmortem_all_levels_pass() {
    let cascade = VerificationCascade::from_ioa(POSTMORTEM_IOA)
        .with_sim_seeds(10)
        .with_prop_test_cases(1000);

    let result = cascade.run();

    for level in &result.levels {
        assert!(
            level.passed,
            "Postmortem cascade level failed: {}",
            level.summary
        );
    }

    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .unwrap()
            .passed,
        "L1 Model Check should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::Simulation)
            .unwrap()
            .passed,
        "L2 Simulation should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::PropertyTest)
            .unwrap()
            .passed,
        "L3 Property Tests should pass"
    );
    assert!(
        result.all_passed,
        "Postmortem cascade should pass all levels"
    );
}
