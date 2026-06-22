//! Tests for [`Guard`] evaluation (`check`, `check_detailed`).

use super::*;

#[test]
fn guard_always_passes() {
    let guard = Guard::Always;
    let ctx = EvalContext::default();
    assert!(guard.check("Draft", &ctx));
}

#[test]
fn guard_state_in_matches() {
    let guard = Guard::StateIn(vec!["Draft".to_string(), "Active".to_string()]);
    let ctx = EvalContext::default();
    assert!(guard.check("Draft", &ctx));
}

#[test]
fn guard_state_in_no_match() {
    let guard = Guard::StateIn(vec!["Draft".to_string()]);
    let ctx = EvalContext::default();
    assert!(!guard.check("Active", &ctx));
}

#[test]
fn guard_item_count_min_passes() {
    let guard = Guard::ItemCountMin(2);
    let mut ctx = EvalContext::default();
    ctx.counters.insert("items".to_string(), 3);
    assert!(guard.check("Draft", &ctx));
}

#[test]
fn guard_item_count_min_fails() {
    let guard = Guard::ItemCountMin(2);
    let mut ctx = EvalContext::default();
    ctx.counters.insert("items".to_string(), 1);
    assert!(!guard.check("Draft", &ctx));
}

#[test]
fn guard_counter_min_passes() {
    let guard = Guard::CounterMin {
        var: "cycles".to_string(),
        min: 2,
    };
    let mut ctx = EvalContext::default();
    ctx.counters.insert("cycles".to_string(), 3);
    assert!(guard.check("Draft", &ctx));
}

#[test]
fn guard_counter_max_passes() {
    let guard = Guard::CounterMax {
        var: "retries".to_string(),
        max: 3,
    };
    let mut ctx = EvalContext::default();
    ctx.counters.insert("retries".to_string(), 2);
    assert!(guard.check("Draft", &ctx));
}

#[test]
fn guard_counter_max_fails() {
    let guard = Guard::CounterMax {
        var: "retries".to_string(),
        max: 3,
    };
    let mut ctx = EvalContext::default();
    ctx.counters.insert("retries".to_string(), 3);
    assert!(!guard.check("Draft", &ctx));
}

#[test]
fn guard_bool_true_passes() {
    let guard = Guard::BoolTrue("assigned".to_string());
    let mut ctx = EvalContext::default();
    ctx.booleans.insert("assigned".to_string(), true);
    assert!(guard.check("Draft", &ctx));
}

#[test]
fn guard_bool_true_fails_missing() {
    let guard = Guard::BoolTrue("assigned".to_string());
    let ctx = EvalContext::default();
    assert!(!guard.check("Draft", &ctx));
}

#[test]
fn guard_and_all_pass() {
    let guard = Guard::And(vec![
        Guard::Always,
        Guard::StateIn(vec!["Draft".to_string()]),
    ]);
    let ctx = EvalContext::default();
    assert!(guard.check("Draft", &ctx));
}

#[test]
fn guard_and_one_fails() {
    let guard = Guard::And(vec![
        Guard::Always,
        Guard::StateIn(vec!["Active".to_string()]),
    ]);
    let ctx = EvalContext::default();
    assert!(!guard.check("Draft", &ctx));
}

// ---------------------------------------------------------------------
// check_detailed (ADR-0151)
// ---------------------------------------------------------------------

#[test]
fn check_detailed_counter_min_carries_var_required_found() {
    let guard = Guard::CounterMin {
        var: "cycles".to_string(),
        min: 2,
    };
    let mut ctx = EvalContext::default();
    ctx.counters.insert("cycles".to_string(), 1);

    let failure = guard.check_detailed("Draft", &ctx).expect("should fail");
    assert_eq!(failure.kind, GuardFailureKind::CounterMin);
    assert_eq!(failure.var.as_deref(), Some("cycles"));
    assert_eq!(failure.required.as_deref(), Some(">= 2"));
    assert_eq!(failure.found.as_deref(), Some("1"));
}

#[test]
fn check_detailed_bool_distinguishes_missing_from_false() {
    let guard = Guard::BoolTrue("assigned".to_string());

    // Missing -> "<missing>"
    let ctx = EvalContext::default();
    let failure = guard.check_detailed("Draft", &ctx).expect("should fail");
    assert_eq!(failure.kind, GuardFailureKind::BoolTrue);
    assert_eq!(failure.var.as_deref(), Some("assigned"));
    assert_eq!(failure.found.as_deref(), Some("<missing>"));

    // Explicit false -> "false"
    let mut ctx_false = EvalContext::default();
    ctx_false.booleans.insert("assigned".to_string(), false);
    let failure = guard
        .check_detailed("Draft", &ctx_false)
        .expect("should fail");
    assert_eq!(failure.found.as_deref(), Some("false"));
}

#[test]
fn check_detailed_state_in_carries_current_state() {
    let guard = Guard::StateIn(vec!["Draft".to_string(), "Ready".to_string()]);
    let ctx = EvalContext::default();

    let failure = guard.check_detailed("Active", &ctx).expect("should fail");
    assert_eq!(failure.kind, GuardFailureKind::StateIn);
    assert_eq!(failure.required.as_deref(), Some("state in [Draft,Ready]"));
    assert_eq!(failure.found.as_deref(), Some("Active"));
}

#[test]
fn check_detailed_cross_entity_names_entity_and_ref() {
    let guard = Guard::CrossEntityStateIn {
        entity_type: "File".to_string(),
        entity_id_source: "landing_file_id".to_string(),
        required_status: vec!["Ready".to_string(), "Locked".to_string()],
        required: false,
    };
    let ctx = EvalContext::default();

    let failure = guard.check_detailed("Draft", &ctx).expect("should fail");
    assert_eq!(failure.kind, GuardFailureKind::CrossEntityState);
    assert_eq!(failure.var.as_deref(), Some("landing_file_id"));
    assert_eq!(
        failure.required.as_deref(),
        Some("File status in [Ready,Locked]")
    );
}

#[test]
fn check_detailed_and_reports_first_failure_in_source_order() {
    // First conjunct passes, second fails, third would also fail: the
    // detailed failure must name the second (first failing) one.
    let guard = Guard::And(vec![
        Guard::StateIn(vec!["Draft".to_string()]),
        Guard::CounterMin {
            var: "cycles".to_string(),
            min: 2,
        },
        Guard::BoolTrue("approved".to_string()),
    ]);
    let mut ctx = EvalContext::default();
    ctx.counters.insert("cycles".to_string(), 0);

    let failure = guard.check_detailed("Draft", &ctx).expect("should fail");
    assert_eq!(failure.kind, GuardFailureKind::CounterMin);
    assert_eq!(failure.var.as_deref(), Some("cycles"));
}

#[test]
fn check_detailed_returns_none_when_guard_passes() {
    let guard = Guard::And(vec![
        Guard::StateIn(vec!["Draft".to_string()]),
        Guard::CounterMin {
            var: "cycles".to_string(),
            min: 2,
        },
    ]);
    let mut ctx = EvalContext::default();
    ctx.counters.insert("cycles".to_string(), 5);

    assert!(guard.check_detailed("Draft", &ctx).is_none());
}

#[test]
fn check_and_check_detailed_agree_on_pass_fail() {
    let guard = Guard::And(vec![
        Guard::CounterMin {
            var: "a".to_string(),
            min: 1,
        },
        Guard::BoolFalse("locked".to_string()),
    ]);
    let mut ctx = EvalContext::default();
    ctx.counters.insert("a".to_string(), 1);
    // passes
    assert_eq!(
        guard.check("S", &ctx),
        guard.check_detailed("S", &ctx).is_none()
    );
    // now fail the bool
    ctx.booleans.insert("locked".to_string(), true);
    assert_eq!(
        guard.check("S", &ctx),
        guard.check_detailed("S", &ctx).is_none()
    );
}
