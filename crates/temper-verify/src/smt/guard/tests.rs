use super::*;
use crate::model::{ResolvedTransition, build_model_from_ioa};
use temper_spec::predicate::parse;

const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");

macro_rules! platform_spec {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!(
                "../../../../temper-platform/src/specs/",
                $name,
                ".ioa.toml"
            )),
        )
    };
}

// These are the exact thirteen system and ten agent bootstrap sources.
const PLATFORM_SPECS: &[(&str, &str)] = &[
    platform_spec!("Project"),
    platform_spec!("Tenant"),
    platform_spec!("CatalogEntry"),
    platform_spec!("Collaborator"),
    platform_spec!("Version"),
    platform_spec!("Observation"),
    platform_spec!("Problem"),
    platform_spec!("Analysis"),
    platform_spec!("EvolutionDecision"),
    platform_spec!("Insight"),
    platform_spec!("FeatureRequest"),
    platform_spec!("GovernanceDecision"),
    platform_spec!("HttpEndpoint"),
    platform_spec!("agent"),
    platform_spec!("agent_type"),
    platform_spec!("plan"),
    platform_spec!("task"),
    platform_spec!("tool_call"),
    platform_spec!("schedule"),
    platform_spec!("policy"),
    platform_spec!("agent_credential"),
    platform_spec!("trusted_issuer"),
    platform_spec!("principal_generation"),
];

/// The original implementation: every query owns a fresh independent solver.
fn independent_guard_queries(model: &TemperModel, max_counter: usize) -> Vec<(String, bool)> {
    model
        .transitions
        .iter()
        .map(|t| {
            let solver = Solver::new();

            // Check that at least one from_state exists in the state space
            if !t.from_states.is_empty() {
                let has_valid_from = t.from_states.iter().any(|s| model.states.contains(s));
                if !has_valid_from {
                    return (t.name.clone(), false);
                }
            }

            // Create Z3 variables for each counter, bounded [0, max_counter]
            let counter_vars = make_counter_vars(model, &solver, max_counter);
            let bool_vars = make_bool_vars(model);
            let list_vars = make_list_vars(model, &solver, max_counter);
            let status_var = make_status_var(model, &solver);

            if !t.from_states.is_empty() {
                let from_formula = encode_state_membership(&status_var, &t.from_states, model);
                solver.assert(&from_formula);
            }

            // Encode the guard as a Z3 formula and assert it
            let symbols = Symbols {
                counters: counter_vars.into_iter().collect(),
                bools: bool_vars.into_iter().collect(),
                lists: &list_vars,
                status: status_var,
                prefix: "",
            };
            solver.assert(encode(&t.guard, &symbols, model));

            let sat = matches!(solver.check(), SatResult::Sat);
            (t.name.clone(), sat)
        })
        .collect()
}

#[test]
fn guards_match_independent_queries_for_all_bootstrap_specs_and_order() {
    assert_eq!(PLATFORM_SPECS.len(), 23);
    for (name, source) in PLATFORM_SPECS
        .iter()
        .copied()
        .chain(std::iter::once(("Order", ORDER_IOA)))
    {
        for bound in [0, 1, 2] {
            let mut model = build_model_from_ioa(source, bound).unwrap();
            for reverse in [false, true] {
                if reverse {
                    model.transitions.reverse();
                }
                assert_eq!(
                    check_guard_satisfiability(&model, bound),
                    independent_guard_queries(&model, bound),
                    "{name}, bound={bound}, reversed={reverse}"
                );
            }
        }
    }
}

fn transition(name: &str, from: &[&str], guard: &str) -> ResolvedTransition {
    ResolvedTransition {
        name: name.to_owned(),
        from_states: from.iter().map(|state| (*state).to_owned()).collect(),
        to_state: None,
        guard: parse(guard).unwrap(),
        effects: Vec::new(),
        params: BTreeMap::new(),
    }
}

fn assert_cases(cases: &[(&str, &[&str], &str, bool)], bound: usize) {
    let mut model = build_model_from_ioa(ORDER_IOA, bound).unwrap();
    model.initial_lists.insert("labels".to_owned(), Vec::new());
    model.transitions = cases
        .iter()
        .map(|(name, from, guard, _)| transition(name, from, guard))
        .collect();
    let mut expected: Vec<_> = cases
        .iter()
        .map(|(name, _, _, sat)| ((*name).to_owned(), *sat))
        .collect();

    for reverse in [false, true] {
        if reverse {
            model.transitions.reverse();
            expected.reverse();
        }
        assert_eq!(independent_guard_queries(&model, bound), expected);
        assert_eq!(check_guard_satisfiability(&model, bound), expected);
    }
}

#[test]
fn counter_bounds_and_previous_sat_or_unsat_queries_do_not_leak() {
    assert_cases(
        &[
            ("CounterZero", &["Draft"], "items == 0", true),
            ("CounterOne", &["Draft"], "items == 1", true),
            ("OutOfBounds", &[], "items > 1", false),
            ("AfterUnsat", &[], "items == 1", true),
            ("NegativeCounter", &[], "items < 0", false),
            ("AfterNegative", &[], "items == 0", true),
        ],
        1,
    );
}

#[test]
fn boolean_and_free_atoms_are_independent_between_queries() {
    assert_cases(
        &[
            ("AddressPresent", &[], "has_address", true),
            ("AddressAbsent", &[], "!has_address", true),
            ("Contradiction", &[], "has_address && !has_address", false),
            ("AfterContradiction", &[], "has_address", true),
            ("FreeAtom", &[], "unknown_field == 'ready'", true),
            ("FreeAtomNegated", &[], "!(unknown_field == 'ready')", true),
        ],
        1,
    );
}

#[test]
fn status_membership_and_invalid_from_states_do_not_constrain_later_queries() {
    assert_cases(
        &[
            ("DraftStatus", &["Draft"], "status == 'Draft'", true),
            (
                "SubmittedStatus",
                &["Submitted"],
                "status == 'Submitted'",
                true,
            ),
            (
                "ConflictingStatus",
                &["Draft"],
                "status == 'Submitted'",
                false,
            ),
            ("AfterUnsat", &["Submitted"], "status == 'Submitted'", true),
            ("InvalidFrom", &["NotAState"], "true", false),
            ("MixedFrom", &["NotAState", "Draft"], "true", true),
            ("Unrestricted", &[], "status == 'Draft'", true),
            ("UndeclaredStatus", &[], "status == 'NotAState'", false),
        ],
        1,
    );
}

#[test]
fn status_index_domain_bounds_are_restored_for_unrestricted_queries() {
    let mut model = build_model_from_ioa(ORDER_IOA, 1).unwrap();
    model.states = vec!["Draft".to_owned(), "Submitted".to_owned()];
    model.transitions = vec![
        transition(
            "OutsideStatusDomain",
            &[],
            "status not in ['Draft', 'Submitted']",
        ),
        transition("InsideStatusDomain", &[], "status == 'Draft'"),
    ];
    let mut expected = vec![
        ("OutsideStatusDomain".to_owned(), false),
        ("InsideStatusDomain".to_owned(), true),
    ];
    for reverse in [false, true] {
        if reverse {
            model.transitions.reverse();
            expected.reverse();
        }
        assert_eq!(independent_guard_queries(&model, 1), expected);
        assert_eq!(check_guard_satisfiability(&model, 1), expected);
    }

    model.states.clear();
    model.transitions = vec![transition("EmptyStateDomain", &[], "true")];
    let expected = vec![("EmptyStateDomain".to_owned(), true)];
    assert_eq!(independent_guard_queries(&model, 1), expected);
    assert_eq!(check_guard_satisfiability(&model, 1), expected);
}

#[test]
fn exact_list_slots_and_length_bounds_are_restored_for_each_query() {
    assert_cases(
        &[
            (
                "TwoDistinctValues",
                &[],
                "'urgent' in labels && 'normal' in labels",
                false,
            ),
            ("UrgentOnly", &[], "'urgent' in labels", true),
            ("NormalOnly", &[], "'normal' in labels", true),
            ("TooLong", &[], "len(labels) > 1", false),
            ("EmptyAfterUnsat", &[], "empty(labels)", true),
            ("NegativeLength", &[], "len(labels) < 0", false),
            ("NonemptyAfterUnsat", &[], "!empty(labels)", true),
        ],
        1,
    );
    assert_cases(
        &[
            ("ContainsAtZeroBound", &[], "'urgent' in labels", false),
            ("EmptyAtZeroBound", &[], "empty(labels)", true),
            ("NonemptyAtZeroBound", &[], "!empty(labels)", false),
        ],
        0,
    );
}

#[test]
fn empty_or_all_invalid_models_do_not_prepare_unbounded_list_slots() {
    let mut model = build_model_from_ioa(ORDER_IOA, 1).unwrap();
    model.initial_lists.insert("labels".to_owned(), Vec::new());
    model.transitions.clear();
    assert!(check_guard_satisfiability(&model, usize::MAX).is_empty());
    assert!(independent_guard_queries(&model, usize::MAX).is_empty());

    model.transitions = vec![
        transition("FirstInvalid", &["MissingFirst"], "true"),
        transition("SecondInvalid", &["MissingSecond"], "'urgent' in labels"),
    ];
    let expected = vec![
        ("FirstInvalid".to_owned(), false),
        ("SecondInvalid".to_owned(), false),
    ];
    assert_eq!(check_guard_satisfiability(&model, usize::MAX), expected);
    assert_eq!(independent_guard_queries(&model, usize::MAX), expected);
}
