//! Tests for the shared guard/effect/invariant semantics.

use super::*;

use super::*;
use std::collections::BTreeMap;

fn state(status: &str) -> TemperModelState {
    TemperModelState {
        status: status.to_string(),
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
    }
}

#[test]
fn guard_always_passes() {
    assert!(evaluate_guard(&ModelGuard::Always, &state("Any")));
}

#[test]
fn guard_state_in_matches() {
    let g = ModelGuard::StateIn(vec!["Draft".into(), "Active".into()]);
    assert!(evaluate_guard(&g, &state("Draft")));
    assert!(evaluate_guard(&g, &state("Active")));
    assert!(!evaluate_guard(&g, &state("Closed")));
}

#[test]
fn guard_counter_min() {
    let g = ModelGuard::CounterMin {
        var: "items".into(),
        min: 2,
    };
    let mut s = state("A");
    assert!(!evaluate_guard(&g, &s)); // missing counter defaults to 0
    s.counters.insert("items".into(), 1);
    assert!(!evaluate_guard(&g, &s));
    s.counters.insert("items".into(), 2);
    assert!(evaluate_guard(&g, &s));
    s.counters.insert("items".into(), 5);
    assert!(evaluate_guard(&g, &s));
}

#[test]
fn guard_counter_max() {
    let g = ModelGuard::CounterMax {
        var: "items".into(),
        max: 3,
    };
    let mut s = state("A");
    assert!(evaluate_guard(&g, &s)); // 0 < 3
    s.counters.insert("items".into(), 2);
    assert!(evaluate_guard(&g, &s)); // 2 < 3
    s.counters.insert("items".into(), 3);
    assert!(!evaluate_guard(&g, &s)); // 3 < 3 is false
}

#[test]
fn guard_bool_true() {
    let g = ModelGuard::BoolTrue("ready".into());
    let mut s = state("A");
    assert!(!evaluate_guard(&g, &s)); // missing defaults to false
    s.booleans.insert("ready".into(), false);
    assert!(!evaluate_guard(&g, &s));
    s.booleans.insert("ready".into(), true);
    assert!(evaluate_guard(&g, &s));
}

#[test]
fn guard_list_contains() {
    let g = ModelGuard::ListContains {
        var: "tags".into(),
        value: "vip".into(),
    };
    let mut s = state("A");
    assert!(!evaluate_guard(&g, &s)); // no list
    s.lists.insert("tags".into(), vec!["basic".into()]);
    assert!(!evaluate_guard(&g, &s));
    s.lists
        .insert("tags".into(), vec!["vip".into(), "basic".into()]);
    assert!(evaluate_guard(&g, &s));
}

#[test]
fn guard_list_length_min() {
    let g = ModelGuard::ListLengthMin {
        var: "items".into(),
        min: 2,
    };
    let mut s = state("A");
    assert!(!evaluate_guard(&g, &s));
    s.lists.insert("items".into(), vec!["a".into()]);
    assert!(!evaluate_guard(&g, &s));
    s.lists.insert("items".into(), vec!["a".into(), "b".into()]);
    assert!(evaluate_guard(&g, &s));
}

#[test]
fn guard_and_all_must_pass() {
    let g = ModelGuard::And(vec![
        ModelGuard::StateIn(vec!["Draft".into()]),
        ModelGuard::CounterMin {
            var: "items".into(),
            min: 1,
        },
    ]);
    let mut s = state("Draft");
    assert!(!evaluate_guard(&g, &s)); // counter fails
    s.counters.insert("items".into(), 1);
    assert!(evaluate_guard(&g, &s));
    s.status = "Active".into();
    assert!(!evaluate_guard(&g, &s)); // state fails
}

#[test]
fn effect_increment_counter() {
    let mut s = state("A");
    apply_effects(&[ModelEffect::IncrementCounter("x".into())], &mut s, "Act");
    assert_eq!(s.counters["x"], 1);
    apply_effects(&[ModelEffect::IncrementCounter("x".into())], &mut s, "Act");
    assert_eq!(s.counters["x"], 2);
}

#[test]
fn effect_decrement_counter_saturates_at_zero() {
    let mut s = state("A");
    apply_effects(&[ModelEffect::DecrementCounter("x".into())], &mut s, "Act");
    assert_eq!(s.counters["x"], 0); // saturating sub from 0
    s.counters.insert("x".into(), 3);
    apply_effects(&[ModelEffect::DecrementCounter("x".into())], &mut s, "Act");
    assert_eq!(s.counters["x"], 2);
}

#[test]
fn effect_set_bool() {
    let mut s = state("A");
    apply_effects(
        &[ModelEffect::SetBool {
            var: "done".into(),
            value: true,
        }],
        &mut s,
        "Act",
    );
    assert!(s.booleans["done"]);
    apply_effects(
        &[ModelEffect::SetBool {
            var: "done".into(),
            value: false,
        }],
        &mut s,
        "Act",
    );
    assert!(!s.booleans["done"]);
}

#[test]
fn effect_list_append() {
    let mut s = state("A");
    apply_effects(&[ModelEffect::ListAppend("log".into())], &mut s, "AddItem");
    assert_eq!(s.lists["log"], vec!["AddItem#1"]);
    apply_effects(&[ModelEffect::ListAppend("log".into())], &mut s, "AddItem");
    assert_eq!(s.lists["log"], vec!["AddItem#1", "AddItem#2"]);
}

#[test]
fn effect_list_remove_at() {
    let mut s = state("A");
    s.lists
        .insert("log".into(), vec!["a".into(), "b".into(), "c".into()]);
    apply_effects(&[ModelEffect::ListRemoveAt("log".into())], &mut s, "Act");
    assert_eq!(s.lists["log"], vec!["b", "c"]);
}

#[test]
fn effect_list_remove_at_empty_is_noop() {
    let mut s = state("A");
    s.lists.insert("log".into(), vec![]);
    apply_effects(&[ModelEffect::ListRemoveAt("log".into())], &mut s, "Act");
    assert!(s.lists["log"].is_empty());
}

// -----------------------------------------------------------------------
// evaluate_invariant_kind — every InvariantKind arm
// -----------------------------------------------------------------------

use super::super::types::ResolvedTransition;

/// Build a minimal model with the given states and transitions.
fn model_with(states: &[&str], transitions: Vec<ResolvedTransition>) -> TemperModel {
    TemperModel {
        states: states.iter().map(|s| s.to_string()).collect(),
        transitions,
        invariants: vec![],
        liveness: vec![],
        initial_status: states.first().map(|s| s.to_string()).unwrap_or_default(),
        initial_counters: BTreeMap::new(),
        initial_booleans: BTreeMap::new(),
        initial_lists: BTreeMap::new(),
        counter_bounds: BTreeMap::new(),
        default_max_counter: 2,
    }
}

fn transition(name: &str, from: &str, effects: Vec<ModelEffect>) -> ResolvedTransition {
    ResolvedTransition {
        name: name.to_string(),
        from_states: vec![from.to_string()],
        to_state: None,
        guard: ModelGuard::Always,
        effects,
    }
}

/// Evaluate with no required states, in EnabledActions mode.
fn holds(kind: &InvariantKind, model: &TemperModel, state: &TemperModelState) -> bool {
    evaluate_invariant_kind(
        kind,
        &[],
        model,
        state,
        NoFurtherTransitionsMode::EnabledActions,
    )
}

#[test]
fn invariant_status_in_set() {
    let model = model_with(&["A", "B"], vec![]);
    assert!(holds(&InvariantKind::StatusInSet, &model, &state("A")));
    assert!(holds(&InvariantKind::StatusInSet, &model, &state("B")));
    assert!(!holds(&InvariantKind::StatusInSet, &model, &state("Zzz")));
}

#[test]
fn invariant_counter_positive() {
    let model = model_with(&["A"], vec![]);
    let kind = InvariantKind::CounterPositive { var: "n".into() };
    let mut s = state("A");
    assert!(!holds(&kind, &model, &s)); // missing defaults to 0
    s.counters.insert("n".into(), 0);
    assert!(!holds(&kind, &model, &s));
    s.counters.insert("n".into(), 1);
    assert!(holds(&kind, &model, &s));
}

#[test]
fn invariant_bool_required() {
    let model = model_with(&["A"], vec![]);
    let expect_true = InvariantKind::BoolRequired {
        var: "ready".into(),
        expect: true,
    };
    let expect_false = InvariantKind::BoolRequired {
        var: "ready".into(),
        expect: false,
    };
    let mut s = state("A");
    assert!(!holds(&expect_true, &model, &s)); // missing defaults to false
    assert!(holds(&expect_false, &model, &s));
    s.booleans.insert("ready".into(), true);
    assert!(holds(&expect_true, &model, &s));
    assert!(!holds(&expect_false, &model, &s));
}

#[test]
fn invariant_no_further_transitions_both_modes_agree_without_budgets() {
    let model = model_with(&["A", "B"], vec![transition("Go", "A", vec![])]);
    let kind = InvariantKind::NoFurtherTransitions;
    for mode in [
        NoFurtherTransitionsMode::EnabledActions,
        NoFurtherTransitionsMode::GuardOnly,
    ] {
        // "Go" is enabled from A: invariant violated.
        assert!(!evaluate_invariant_kind(
            &kind,
            &[],
            &model,
            &state("A"),
            mode
        ));
        // Nothing enabled from B: invariant holds.
        assert!(evaluate_invariant_kind(
            &kind,
            &[],
            &model,
            &state("B"),
            mode
        ));
    }
}

#[test]
fn invariant_no_further_transitions_modes_diverge_at_counter_bound() {
    // "Add" increments counter "n"; default_max_counter is 2, so at
    // n == 2, Model::actions suppresses it but the guard still holds.
    let model = model_with(
        &["A"],
        vec![transition(
            "Add",
            "A",
            vec![ModelEffect::IncrementCounter("n".into())],
        )],
    );
    let kind = InvariantKind::NoFurtherTransitions;
    let mut s = state("A");
    s.counters.insert("n".into(), 2);
    // EnabledActions: bound-blocked transition is not enabled — holds.
    assert!(evaluate_invariant_kind(
        &kind,
        &[],
        &model,
        &s,
        NoFurtherTransitionsMode::EnabledActions
    ));
    // GuardOnly: budgets ignored, transition counts as enabled — violated.
    assert!(!evaluate_invariant_kind(
        &kind,
        &[],
        &model,
        &s,
        NoFurtherTransitionsMode::GuardOnly
    ));
}

#[test]
fn invariant_implication() {
    let model = model_with(&["A", "B", "C"], vec![]);
    let kind = InvariantKind::Implication;
    let eval = |required: &[String], status: &str| {
        evaluate_invariant_kind(
            &kind,
            required,
            &model,
            &state(status),
            NoFurtherTransitionsMode::EnabledActions,
        )
    };
    // No required states: trivially true.
    assert!(eval(&[], "A"));
    // Required states not in model.states are filtered out: trivially true.
    assert!(eval(&["NotAState".to_string()], "A"));
    // Status in valid required set: holds.
    assert!(eval(&["A".to_string(), "B".to_string()], "B"));
    // Status outside valid required set: violated.
    assert!(!eval(&["A".to_string(), "B".to_string()], "C"));
}

#[test]
fn invariant_counter_compare_all_operators() {
    let model = model_with(&["A"], vec![]);
    let mut s = state("A");
    s.counters.insert("n".into(), 3);
    let eval = |op: AssertCompareOp, value: usize| {
        holds(
            &InvariantKind::CounterCompare {
                var: "n".into(),
                op,
                value,
            },
            &model,
            &s,
        )
    };
    assert!(eval(AssertCompareOp::Gt, 2));
    assert!(!eval(AssertCompareOp::Gt, 3));
    assert!(eval(AssertCompareOp::Gte, 3));
    assert!(!eval(AssertCompareOp::Gte, 4));
    assert!(eval(AssertCompareOp::Lt, 4));
    assert!(!eval(AssertCompareOp::Lt, 3));
    assert!(eval(AssertCompareOp::Lte, 3));
    assert!(!eval(AssertCompareOp::Lte, 2));
    assert!(eval(AssertCompareOp::Eq, 3));
    assert!(!eval(AssertCompareOp::Eq, 2));
}

#[test]
fn invariant_counter_compare_missing_counter_defaults_to_zero() {
    let model = model_with(&["A"], vec![]);
    let kind = InvariantKind::CounterCompare {
        var: "missing".into(),
        op: AssertCompareOp::Eq,
        value: 0,
    };
    assert!(holds(&kind, &model, &state("A")));
}

#[test]
fn invariant_never_state() {
    let model = model_with(&["A", "Bad"], vec![]);
    let kind = InvariantKind::NeverState {
        state: "Bad".into(),
    };
    assert!(holds(&kind, &model, &state("A")));
    assert!(!holds(&kind, &model, &state("Bad")));
}

#[test]
fn invariant_and_or_nesting() {
    let model = model_with(&["A", "Bad"], vec![]);
    let nested = InvariantKind::And(vec![
        InvariantKind::NeverState {
            state: "Bad".into(),
        },
        InvariantKind::Or(vec![
            InvariantKind::CounterPositive { var: "n".into() },
            InvariantKind::BoolRequired {
                var: "ready".into(),
                expect: true,
            },
        ]),
    ]);

    // Neither Or branch holds: And fails.
    let mut s = state("A");
    assert!(!holds(&nested, &model, &s));
    // One Or branch holds (counter positive): And passes.
    s.counters.insert("n".into(), 1);
    assert!(holds(&nested, &model, &s));
    // Other Or branch holds (bool set): still passes.
    s.counters.insert("n".into(), 0);
    s.booleans.insert("ready".into(), true);
    assert!(holds(&nested, &model, &s));
    // NeverState leaf fails: And fails regardless of Or.
    s.status = "Bad".into();
    assert!(!holds(&nested, &model, &s));
}

#[test]
fn invariant_unverifiable_always_holds() {
    let model = model_with(&["A"], vec![]);
    let kind = InvariantKind::Unverifiable {
        expression: "len(items) == count".into(),
    };
    assert!(holds(&kind, &model, &state("A")));
    assert!(holds(&kind, &model, &state("Anything")));
}

#[test]
fn collect_list_contains_from_nested_guard() {
    let guard = ModelGuard::And(vec![
        ModelGuard::ListContains {
            var: "tags".into(),
            value: "vip".into(),
        },
        ModelGuard::And(vec![
            ModelGuard::ListContains {
                var: "roles".into(),
                value: "admin".into(),
            },
            ModelGuard::Always,
        ]),
        ModelGuard::CounterMin {
            var: "x".into(),
            min: 1,
        },
    ]);
    let mut pairs = BTreeSet::new();
    collect_list_contains_pairs(&guard, &mut pairs);
    assert_eq!(pairs.len(), 2);
    assert!(pairs.contains(&("tags".into(), "vip".into())));
    assert!(pairs.contains(&("roles".into(), "admin".into())));
}
