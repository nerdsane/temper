//! Cascade unit tests.

use super::*;

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
fn test_cascade_fails_closed_on_unsupported_safety_invariant() {
    // Counter-to-counter comparison is not in the verifier capability set
    // (counter-to-literal only). Must fail closed independent of seeds.
    let unsupported = r#"
[automaton]
name = "Workspace"
states = ["Active", "Archived"]
initial = "Active"

[[state]]
name = "used_bytes"
type = "counter"
initial = "0"

[[state]]
name = "quota_limit"
type = "counter"
initial = "0"

[[action]]
name = "Archive"
from = ["Active"]
to = "Archived"

[[invariant]]
name = "UsageBelowQuota"
when = ["Active"]
assert = "used_bytes <= quota_limit"
"#;
    let result = VerificationCascade::from_ioa(unsupported)
        .with_sim_seeds(3)
        .with_prop_test_cases(20)
        .run();

    assert!(
        !result.all_passed,
        "unsupported safety must not report cascade success"
    );
    assert_eq!(result.unsupported_invariants.len(), 1);
    let diag = &result.unsupported_invariants[0];
    assert_eq!(diag.code, UNSUPPORTED_SAFETY_INVARIANT_CODE);
    assert_eq!(diag.invariant_name, "UsageBelowQuota");
    assert_eq!(diag.expression, "used_bytes <= quota_limit");
    let span = diag
        .source_span
        .as_ref()
        .expect("source span for named invariant");
    assert!(
        span.start_byte < span.end_byte,
        "span should cover the invariant table"
    );
    assert!(span.start_line >= 1);
    let slice = &unsupported[span.start_byte..span.end_byte];
    assert!(
        slice.contains("UsageBelowQuota") && slice.contains("used_bytes <= quota_limit"),
        "span should cover name and assert, got: {slice:?}"
    );
    // Must not be described as a soft skip warning.
    assert!(
        result
            .warnings
            .iter()
            .all(|w| !w.contains("skipped at model level")),
        "unsupported safety must not be warning-only: {:?}",
        result.warnings
    );
}

#[test]
fn test_cascade_unsupported_span_multiline_and_repeated() {
    let src = r#"
[automaton]
name = "Multi"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]
to = "B"

[[invariant]]
name = "FirstBad"
assert = "alpha <= beta"

[[invariant]]
name = "OkNever"
assert = "never(B)"

[[invariant]]
name = "SecondBad"
assert = "gamma + delta"
"#;
    let result = VerificationCascade::from_ioa(src)
        .with_sim_seeds(1)
        .with_prop_test_cases(5)
        .run();
    assert!(!result.all_passed);
    assert_eq!(result.unsupported_invariants.len(), 2);
    assert_eq!(result.unsupported_invariants[0].invariant_name, "FirstBad");
    assert_eq!(result.unsupported_invariants[1].invariant_name, "SecondBad");
    for diag in &result.unsupported_invariants {
        let span = diag.source_span.as_ref().expect("span");
        let slice = &src[span.start_byte..span.end_byte];
        assert!(
            slice.contains(&diag.invariant_name),
            "span for {} must include its name: {slice:?}",
            diag.invariant_name
        );
    }
    // Distinct spans for the two unsupported tables.
    let a = result.unsupported_invariants[0]
        .source_span
        .as_ref()
        .unwrap();
    let b = result.unsupported_invariants[1]
        .source_span
        .as_ref()
        .unwrap();
    assert!(a.end_byte <= b.start_byte || b.end_byte <= a.start_byte);
}

#[test]
fn test_cascade_fully_supported_spec_passes() {
    let result = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(3)
        .with_prop_test_cases(50)
        .run();
    assert!(
        result.unsupported_invariants.is_empty(),
        "ORDER fixture must be fully supported after payment_captured was declared: {:?}",
        result.unsupported_invariants
    );
    assert!(
        result.all_passed,
        "supported ORDER cascade should pass, levels: {:?}",
        result
            .levels
            .iter()
            .map(|l| (&l.summary, l.passed))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_fail_fast_stops_on_unsupported_before_levels() {
    let unsupported = r#"
[automaton]
name = "Bad"
states = ["A"]
initial = "A"

[[invariant]]
name = "Mystery"
assert = "not_a_real_expression(x)"
"#;
    let result = VerificationCascade::from_ioa(unsupported)
        .with_fail_fast()
        .run();
    assert!(!result.all_passed);
    assert!(
        result.levels.is_empty(),
        "fail_fast capability gate should skip level exploration"
    );
    assert_eq!(result.unsupported_invariants.len(), 1);
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

// ─── ADR-0046: composite cascade integration tests ─────────────────

#[test]
fn cascade_reports_composite_when_scope_configured() {
    use temper_spec::automaton::parse_automaton;

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
    use temper_spec::automaton::parse_automaton;
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
