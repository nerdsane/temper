use super::*;

fn kinds() -> BTreeMap<String, VarKind> {
    [
        ("items", VarKind::Counter),
        ("ready", VarKind::Bool),
        ("tags", VarKind::List),
        ("goal", VarKind::Str),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn state(status: &str) -> TemperModelState {
    TemperModelState {
        status: status.to_string(),
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        strings: BTreeMap::new(),
    }
}

fn g(source: &str) -> Expr {
    temper_spec::predicate::parse(source).unwrap()
}

#[test]
fn declared_variables_missing_from_state_read_as_zero_values() {
    let s = state("A");
    let k = kinds();
    assert!(evaluate_guard(&g("items == 0 && items < 3"), &k, &s));
    assert!(evaluate_guard(&g("!ready && len(tags) == 0"), &k, &s));
    assert!(!evaluate_guard(&g("items >= 2"), &k, &s));
}

#[test]
fn evaluates_modeled_state() {
    let mut s = state("Draft");
    s.counters.insert("items".into(), 2);
    s.booleans.insert("ready".into(), true);
    s.lists.insert("tags".into(), vec!["vip".into()]);
    let k = kinds();
    assert!(evaluate_guard(
        &g("status == 'Draft' && items >= 2 && ready && 'vip' in tags"),
        &k,
        &s
    ));
    assert!(!evaluate_guard(&g("status in ['Closed']"), &k, &s));
}

#[test]
fn unmodeled_values_are_free_for_exploration_and_absent_for_enablement() {
    let s = state("A");
    let k = kinds();
    for unknown in [
        "P[p].status == 'Done'",
        "goal != ''",
        "!(P[p].status == 'Done')",
    ] {
        let guard = g(unknown);
        assert!(guard_may_hold(&guard, &k, &s), "{unknown} may hold");
        assert!(
            !evaluate_guard(&guard, &k, &s),
            "{unknown} is not guaranteed"
        );
    }
    // A local condition that is false decides, whatever the unknown is.
    let guard = g("items > 5 && P[p].status == 'Done'");
    assert!(!guard_may_hold(&guard, &k, &s));
    // A local condition that is true decides a disjunction.
    let guard = g("items == 0 || P[p].status == 'Done'");
    assert!(evaluate_guard(&guard, &k, &s));
}

#[test]
fn collects_list_membership_atoms_through_any_connective() {
    let mut pairs = BTreeSet::new();
    collect_list_contains_pairs(&g("!('a' in tags) || ready => 'b' in tags"), &mut pairs);
    assert_eq!(
        pairs.into_iter().collect::<Vec<_>>(),
        vec![("tags".into(), "a".into()), ("tags".into(), "b".into())]
    );
}

fn two_strings_model() -> TemperModel {
    crate::model::build_model_from_ioa(
        r#"
[automaton]
name = "Pair"
states = ["New", "Done"]
initial = "New"
[[state]]
name = "a"
type = "string"
initial = ""
[[state]]
name = "b"
type = "string"
initial = ""
[[action]]
name = "Write"
kind = "input"
from = ["New"]
to = "Done"
params = [{ name = "a", type = "string" }, { name = "b", type = "string" }]
[[invariant]]
name = "equal"
assert = "a == b"
"#,
        3,
    )
    .unwrap()
}

#[test]
fn fresh_parameters_can_differ_from_each_other() {
    let model = two_strings_model();
    let initial = stateright::Model::init_states(&model).remove(0);
    let successors = successors(&model, &model.transitions[0], &initial);
    assert!(successors.iter().any(|(_, s)| !s.strings["a"].is_empty()
        && !s.strings["b"].is_empty()
        && s.strings["a"] != s.strings["b"]));
}

#[test]
fn concrete_hash_literals_are_never_renamed_or_equal_to_symbols() {
    let mut values = BTreeMap::from([
        ("literal".into(), ModelString::from("#2")),
        ("symbol".into(), ModelString::Symbol(2)),
    ]);
    canonical_strings(&mut [&mut values]);
    assert_eq!(values["literal"], ModelString::from("#2"));
    assert_eq!(values["symbol"], ModelString::Symbol(1));
    let mut s = state("New");
    s.strings = values;
    let k = BTreeMap::from([
        ("literal".into(), VarKind::Str),
        ("symbol".into(), VarKind::Str),
    ]);
    assert_eq!(
        truth(
            &g("literal == '#2' && symbol != '#1' && literal != symbol"),
            &k,
            &s
        ),
        Truth::True
    );
}

#[test]
fn string_products_stop_inside_expansion() {
    let model = two_strings_model();
    let initial = stateright::Model::init_states(&model).remove(0);
    let budget = Expansion::default();
    budget.reset(2, None);
    let next = successors_budgeted(
        &model,
        &model.transitions[0],
        &initial,
        &BTreeSet::new(),
        &budget,
    );
    assert!(next.is_empty());
    assert!(budget.exhausted.get());
}

#[test]
fn every_equality_on_a_string_parameter_must_hold() {
    let source = r#"
[automaton]
name = "Equalities"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "a"
type = "string"
initial = ""

[[state]]
name = "b"
type = "string"
initial = "b"

[[action]]
name = "Finish"
kind = "input"
from = ["Open"]
to = "Done"
params = [{ name = "p", type = "string" }]

[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "a"

[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "b"

[[invariant]]
name = "track_both"
assert = "a == b || a != b"
"#;
    let model = crate::model::build_model_from_ioa(source, 2).unwrap();
    let mut initial = stateright::Model::init_states(&model).remove(0);
    let t = &model.transitions[0];
    assert!(!t.string_params[0].writes);
    assert!(
        successors(&model, t, &initial).is_empty(),
        "different fields admit no value for p"
    );
    initial.strings.insert("a".into(), "b".into());
    let allowed = successors(&model, t, &initial);
    assert_eq!(allowed.len(), 1);
    assert_eq!(allowed[0].0["p"], ParamValue::Str("b".into()));
    assert_eq!(allowed[0].1.strings, initial.strings);
}

#[test]
fn nonempty_string_candidates_follow_runtime_whitespace_rules() {
    let source = r#"
[automaton]
name = "Nonempty"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "a"
type = "string"
initial = ""

[[action]]
name = "Finish"
kind = "input"
from = ["Open"]
to = "Done"
params = [{ name = "p", type = "string" }]

[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "a"

[[action.constraints]]
kind = "param_nonempty"
param = "p"

[[invariant]]
name = "track_a"
assert = "a == '' || a != ''"
"#;
    for (name, writes) in [("p", false), ("a", true)] {
        let source = source.replace("\"p\"", &format!("\"{name}\""));
        let model = crate::model::build_model_from_ioa(&source, 2).unwrap();
        let mut initial = stateright::Model::init_states(&model).remove(0);
        let t = &model.transitions[0];
        assert_eq!(t.string_params[0].writes, writes);
        for (value, allowed) in [
            ("", false),
            (" ", false),
            ("\t\n", false),
            ("\u{2003}", false),
            (" b ", true),
            ("b", true),
        ] {
            initial.strings.insert("a".into(), value.into());
            let next = successors(&model, t, &initial);
            assert_eq!(next.len(), usize::from(allowed), "{name} = {value:?}");
            assert_eq!(
                crate::model::parameters::locally_enabled(&model, t, &initial),
                Some(allowed),
                "input availability: {name} = {value:?}"
            );
            if allowed {
                assert_eq!(next[0].0[name], ParamValue::Str(value.into()));
                assert_eq!(next[0].1.strings, initial.strings);
            }
        }
    }
}

#[test]
fn unconstrained_strings_include_distinct_nonempty_blank_values() {
    let model = two_strings_model();
    let initial = stateright::Model::init_states(&model).remove(0);
    let next = successors(&model, &model.transitions[0], &initial);
    assert!(
        next.iter().any(|(_, state)| {
            let a = &state.strings["a"];
            let b = &state.strings["b"];
            !a.is_empty()
                && a.is_blank()
                && !b.is_blank()
                && evaluate_guard(&g("a != b"), &model.var_kinds, state)
        }),
        "blank and nonblank inputs must remain distinct"
    );
    assert!(
        next.iter().any(|(_, state)| {
            let a = &state.strings["a"];
            let b = &state.strings["b"];
            !a.is_empty() && a.is_blank() && !b.is_empty() && b.is_blank() && a != b
        }),
        "different whitespace-only inputs must remain possible"
    );
}

#[test]
fn a_literal_held_elsewhere_reaches_a_parameter_only_as_its_class_says() {
    let offered = |by_class: bool, own: &[&str]| {
        let mut model = two_strings_model();
        model.literals_by_class = by_class;
        model.string_literals.clear();
        model.string_literals.insert(
            "b".into(),
            own.iter().map(|literal| literal.to_string()).collect(),
        );
        let mut held = stateright::Model::init_states(&model).remove(0);
        held.strings.insert("a".into(), "c".into());
        successors(&model, &model.transitions[0], &held)
            .iter()
            .any(|(_, state)| state.strings["b"] == "c".into())
    };
    assert!(
        offered(false, &[]),
        "a model of one type offers what it holds"
    );
    assert!(
        !offered(true, &[]),
        "nothing reading `b` compares or copies it"
    );
    assert!(offered(true, &["c"]), "something reading `b` compares it");
}

#[test]
fn field_and_list_symbols_share_one_canonical_scope() {
    let mut s = state("Ready");
    s.strings.insert("goal".into(), ModelString::Symbol(9));
    s.lists.insert(
        "tags".into(),
        vec![ModelString::Symbol(9), ModelString::Symbol(4), "#1".into()],
    );
    canonical_state(&mut s);
    assert_eq!(s.strings["goal"], ModelString::Symbol(1));
    assert_eq!(
        s.lists["tags"],
        vec![ModelString::Symbol(1), ModelString::Symbol(2), "#1".into()]
    );
    assert!(evaluate_guard(
        &g("goal in tags && '#1' in tags && len(tags) == 3 && !empty(tags)"),
        &kinds(),
        &s
    ));
    s.lists.get_mut("tags").unwrap().remove(0);
    canonical_state(&mut s);
    assert!(!evaluate_guard(&g("goal in tags"), &kinds(), &s));
    assert_eq!(
        string_values(std::iter::once(&s)),
        BTreeSet::from([ModelString::Symbol(1), ModelString::Symbol(2), "#1".into()])
    );
}

#[test]
fn string_choices_and_successor_states_count_against_the_room() {
    let model = two_strings_model();
    let initial = stateright::Model::init_states(&model).remove(0);
    let values = (0..16)
        .map(|i| ModelString::from(format!("{i}{}", "v".repeat(256))))
        .collect();
    let budget = Expansion::default();
    budget.allow(64 << 10);
    let next = successors_budgeted(&model, &model.transitions[0], &initial, &values, &budget);
    assert!(
        budget.short(),
        "input products must stop while they are built"
    );
    drop(next);

    budget.allow(16 << 20);
    let next = successors_budgeted(&model, &model.transitions[0], &initial, &values, &budget);
    assert!(
        !budget.short(),
        "the complete input product fits in a larger room"
    );
    assert_eq!(
        next,
        successors_among(&model, &model.transitions[0], &initial, &values)
    );

    // The input choices fit, but copying this record for each choice does not.
    let mut large = initial;
    large.lists.insert(
        "payload".into(),
        vec![ModelString::from("x".repeat(32 << 10))],
    );
    budget.allow(64 << 10);
    successors_budgeted(
        &model,
        &model.transitions[0],
        &large,
        &BTreeSet::new(),
        &budget,
    );
    assert!(
        budget.short(),
        "successor records must consume room before cloning"
    );
}

#[test]
fn an_allowed_input_still_enables_a_move_past_an_exploration_bound() {
    let spec = r#"
[automaton]
name = "BoundedInput"
states = ["Open", "Done"]
initial = "Open"
[[state]]
name = "count"
type = "counter"
initial = 2
[[state]]
name = "token"
type = "string"
initial = "ok"
[[action]]
name = "Finish"
from = ["Open"]
to = "Done"
params = [{ name = "proof", type = "string" }]
effect = ["count += 1"]
guard = "count >= 0"
[[action.constraints]]
kind = "param_nonempty"
param = "proof"
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "token"
[[invariant]]
name = "TrackToken"
assert = "token == '' || token != ''"
"#;
    let model = crate::model::build_model_from_ioa(spec, 2).unwrap();
    let initial = stateright::Model::init_states(&model).remove(0);
    let transition = &model.transitions[0];
    assert!(successors(&model, transition, &initial).is_empty());
    assert_eq!(
        crate::model::parameters::locally_enabled(&model, transition, &initial),
        Some(true),
        "effects past the bound do not prevent an allowed runtime input"
    );
}
