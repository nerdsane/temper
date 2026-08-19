use super::*;
use temper_spec::automaton::parse_automaton;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

#[test]
fn test_full_cascade_passes_ioa() {
    let cascade = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(5)
        .with_prop_test_cases(100);

    let result = cascade.run();
    for level in &result.levels {
        assert!(level.passed, "IOA cascade level failed: {}", level.summary);
    }
    // L0 + L1 + L2 + L3 = 4 levels
    assert_eq!(result.levels.len(), 4);
}

#[test]
fn test_cascade_from_parsed_automaton() {
    let automaton = parse_automaton(ORDER_IOA).expect("order fixture parses");
    let result = VerificationCascade::from_automaton(automaton)
        .with_sim_seeds(3)
        .with_prop_test_cases(50)
        .run();
    assert!(result.all_passed);
    assert_eq!(result.levels.len(), 4);
}

#[test]
fn test_try_from_ioa_rejects_tla() {
    let tla = "---- MODULE Order ----\nVARIABLE status\n====\n";
    let err = match VerificationCascade::try_from_ioa(tla) {
        Ok(_) => panic!("TLA+ must not parse as IOA"),
        Err(e) => e,
    };
    assert!(
        err.contains("failed to parse I/O Automaton TOML"),
        "expected parse error, got: {err}"
    );
}

#[test]
fn test_cascade_has_all_levels() {
    let cascade = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(3)
        .with_prop_test_cases(50);

    let result = cascade.run();

    assert!(
        result
            .level_result(CascadeLevel::SymbolicVerification)
            .is_some()
    );
    assert!(result.level_result(CascadeLevel::ModelCheck).is_some());
    assert!(result.level_result(CascadeLevel::Simulation).is_some());
    assert!(result.level_result(CascadeLevel::PropertyTest).is_some());
}

#[test]
fn test_cascade_level_summaries() {
    let cascade = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(3)
        .with_prop_test_cases(50);

    let result = cascade.run();

    let l0 = result
        .level_result(CascadeLevel::SymbolicVerification)
        .unwrap();
    assert!(l0.summary.contains("L0"), "Should have L0 prefix");
    assert!(l0.passed);

    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    assert!(l1.summary.contains("L1"), "Should have L1 prefix");
    assert!(l1.passed);

    let l2 = result.level_result(CascadeLevel::Simulation).unwrap();
    assert!(l2.summary.contains("L2"), "Should have L2 prefix");
    assert!(l2.passed);

    let l3 = result.level_result(CascadeLevel::PropertyTest).unwrap();
    assert!(l3.summary.contains("L3"), "Should have L3 prefix");
    assert!(l3.passed);
}

#[test]
fn test_cascade_warnings_for_unverifiable_invariants() {
    let cascade = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(3)
        .with_prop_test_cases(50);

    let result = cascade.run();
    // Order spec has "payment_captured" which is not a declared bool,
    // so ShipRequiresPayment becomes Unverifiable.
    assert!(
        !result.warnings.is_empty(),
        "Should have warnings for unverifiable invariants"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("ShipRequiresPayment")),
        "Should warn about ShipRequiresPayment, got: {:?}",
        result.warnings,
    );
}

#[test]
fn test_fail_fast_stops_at_first_failure() {
    // Use a spec that will fail L0 (dead guard).
    let broken_spec = r#"
[automaton]
name = "Broken"
states = ["A", "B"]
initial = "A"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[action]]
name = "Go"
from = ["A"]
to = "B"
guard = "count > 9"
"#;
    let cascade = VerificationCascade::from_ioa(broken_spec)
        .with_sim_seeds(1)
        .with_prop_test_cases(10)
        .with_fail_fast();

    let result = cascade.run();
    assert!(!result.all_passed);
    // Should have stopped early — fewer than 4 levels.
    assert!(
        result.levels.len() < 4,
        "fail_fast should stop early, got {} levels",
        result.levels.len(),
    );
}

#[test]
fn test_no_fail_fast_runs_all_levels() {
    let cascade = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(3)
        .with_prop_test_cases(50);

    let result = cascade.run();
    // Without fail_fast, all 4 levels should run.
    assert_eq!(result.levels.len(), 4);
}

#[test]
fn cascade_reports_composite_when_scope_configured() {
    let order_spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "confirm_triggers_auth"
kind = "entity"
principal = "payment-service"
target_entity = "Payment"
target_action = "AuthorizePayment"

[action.triggers.resolve_target]
type = "same_id"
"#;
    let payment_spec = r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
"#;
    let order = parse_automaton(order_spec).unwrap();
    let payment = parse_automaton(payment_spec).unwrap();

    let cascade = VerificationCascade::from_ioa(order_spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(10)
        .with_composite_scope(vec![order, payment], "Order");

    let result = cascade.run();
    let report = result
        .composite_report
        .expect("composite scope was configured");
    assert_eq!(report.seed, "Order");
    assert!(report.scope.contains(&"Order".to_string()));
    assert!(report.scope.contains(&"Payment".to_string()));
    assert_eq!(report.edge_count, 1);
    assert!(!report.has_cycle);
    assert!(report.summary.contains("Order"));
}

#[test]
fn cascade_without_composite_scope_has_none_report() {
    let cascade = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(2)
        .with_prop_test_cases(10);
    let result = cascade.run();
    assert!(result.composite_report.is_none());
}

#[test]
fn cascade_composite_missing_seed_records_warning_not_failure() {
    let order_spec = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "A"
from = ["Draft"]
"#;
    let order = parse_automaton(order_spec).unwrap();

    let cascade = VerificationCascade::from_ioa(order_spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(10)
        .with_composite_scope(vec![order], "NotAnEntity");

    let result = cascade.run();
    assert!(result.composite_report.is_none());
    assert!(
        result.warnings.iter().any(|w| w.contains("NotAnEntity")),
        "warning should mention missing seed. Got: {:?}",
        result.warnings
    );
}
