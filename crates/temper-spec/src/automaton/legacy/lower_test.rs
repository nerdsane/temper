//! Differential tests: the old evaluators (copied here as references) and
//! `eval(lower(x))` must agree on every generated state.

use std::collections::BTreeMap;

use super::field_predicate::FieldPredicate;
use super::lower::*;
use super::syntax::Guard;
use crate::predicate::{Env, Expr, Truth, Val, eval};

/// Deterministic LCG so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % bound
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.next(items.len() as u64) as usize]
    }
}

const STATES: [&str; 4] = ["A", "B", "C", "D"];

#[derive(Debug, Default)]
struct State {
    status: String,
    counters: BTreeMap<String, usize>,
    booleans: BTreeMap<String, bool>,
    lists: BTreeMap<String, Vec<String>>,
    /// id field -> resolved statuses of its targets (`None` = not found).
    /// An empty vector is an unset reference.
    cross: BTreeMap<String, Vec<Option<String>>>,
}

impl Env for State {
    fn status(&self) -> Val<'_> {
        Val::Str(&self.status)
    }
    fn var(&self, name: &str) -> Val<'_> {
        if let Some(c) = self.counters.get(name) {
            return Val::Num(*c as f64);
        }
        if let Some(b) = self.booleans.get(name) {
            return Val::Bool(*b);
        }
        if let Some(l) = self.lists.get(name) {
            return Val::Strs(l);
        }
        if let Some(targets) = self.cross.get(name) {
            // The id field itself: unset when there are no targets.
            return if targets.is_empty() {
                Val::Null
            } else {
                Val::Str("id")
            };
        }
        Val::Null
    }
    fn cross_statuses(&self, _entity_type: &str, id_field: &str) -> Option<Vec<Val<'_>>> {
        let targets = self.cross.get(id_field).map(Vec::as_slice).unwrap_or(&[]);
        if targets.is_empty() {
            return Some(vec![Val::Null]);
        }
        Some(
            targets
                .iter()
                .map(|t| t.as_deref().map_or(Val::Null, Val::Str))
                .collect(),
        )
    }
}

/// Reference: `temper_jit::table::Guard::check` plus the server's
/// `resolve_cross_entity_guards` (state/dispatch/cross_entity.rs).
fn reference_guard(guard: &Guard, s: &State) -> bool {
    match guard {
        Guard::StateIn { values } => values.contains(&s.status),
        Guard::MinCount { var, min } => s.counters.get(var).copied().unwrap_or(0) >= *min,
        Guard::MaxCount { var, max } => s.counters.get(var).copied().unwrap_or(0) < *max,
        Guard::IsTrue { var } => s.booleans.get(var).copied().unwrap_or(false),
        Guard::IsFalse { var } => !s.booleans.get(var).copied().unwrap_or(false),
        Guard::ListContains { var, value } => s.lists.get(var).is_some_and(|l| l.contains(value)),
        Guard::ListLengthMin { var, min } => s.lists.get(var).map_or(0, Vec::len) >= *min,
        Guard::CrossEntityState {
            entity_id_source,
            required_status,
            forbidden_status,
            required,
            ..
        } => {
            let targets = s.cross.get(entity_id_source).cloned().unwrap_or_default();
            if targets.is_empty() {
                return !required;
            }
            targets.iter().all(|t| match t {
                Some(status) => {
                    (required_status.is_empty() || required_status.contains(status))
                        && !forbidden_status.contains(status)
                }
                None => required_status.is_empty() && !required,
            })
        }
    }
}

fn gen_statuses(rng: &mut Rng) -> Vec<String> {
    let n = rng.next(3) as usize;
    (0..n).map(|_| rng.pick(&STATES).to_string()).collect()
}

fn gen_guard(rng: &mut Rng) -> Guard {
    match rng.next(8) {
        0 => Guard::StateIn {
            values: gen_statuses(rng),
        },
        1 => Guard::MinCount {
            var: "c".into(),
            min: rng.next(4) as usize,
        },
        2 => Guard::MaxCount {
            var: "c".into(),
            max: rng.next(4) as usize,
        },
        3 => Guard::IsTrue { var: "b".into() },
        4 => Guard::IsFalse { var: "b".into() },
        5 => Guard::ListContains {
            var: "l".into(),
            value: rng.pick(&STATES).to_string(),
        },
        6 => Guard::ListLengthMin {
            var: "l".into(),
            min: rng.next(3) as usize,
        },
        _ => Guard::CrossEntityState {
            entity_type: "T".into(),
            entity_id_source: "ref".into(),
            required_status: gen_statuses(rng),
            forbidden_status: gen_statuses(rng),
            required: rng.next(2) == 0,
        },
    }
}

fn gen_state(rng: &mut Rng) -> State {
    let mut s = State {
        status: rng.pick(&STATES).to_string(),
        ..State::default()
    };
    s.counters.insert("c".into(), rng.next(4) as usize);
    s.booleans.insert("b".into(), rng.next(2) == 0);
    s.lists.insert("l".into(), gen_statuses(rng));
    let targets = (0..rng.next(3))
        .map(|_| (rng.next(4) > 0).then(|| rng.pick(&STATES).to_string()))
        .collect();
    s.cross.insert("ref".into(), targets);
    s
}

#[test]
fn lowered_action_guards_match_the_old_evaluator() {
    let mut rng = Rng(0xa11ce);
    for case in 0..20_000 {
        let guards: Vec<Guard> = (0..=rng.next(3)).map(|_| gen_guard(&mut rng)).collect();
        let expr = guards_to_expr(&guards);
        // The printed form must parse back to the same tree.
        assert_eq!(
            crate::predicate::parse(&expr.to_string()).as_ref(),
            Ok(&expr)
        );
        for _ in 0..4 {
            let state = gen_state(&mut rng);
            let old = guards.iter().all(|g| reference_guard(g, &state));
            let new = eval(&expr, &state);
            assert_eq!(
                new,
                if old { Truth::True } else { Truth::False },
                "case {case}: {guards:?} => `{expr}` on {state:?}"
            );
        }
    }
}

fn gen_predicate(rng: &mut Rng, depth: u32) -> FieldPredicate {
    let fields = ["x", "y"];
    let leaf = depth == 0 || rng.next(2) == 0;
    if leaf {
        let field = rng.pick(&fields).to_string();
        return match rng.next(3) {
            0 => FieldPredicate::Absent { field, absent: () },
            1 => FieldPredicate::Empty { field, empty: () },
            _ => FieldPredicate::Equals {
                field,
                equals: rng
                    .pick(&[
                        serde_json::json!("a"),
                        serde_json::json!(true),
                        serde_json::json!(false),
                        serde_json::json!(1),
                    ])
                    .clone(),
            },
        };
    }
    match rng.next(3) {
        0 => FieldPredicate::AnyOf {
            any_of: vec![gen_predicate(rng, depth - 1), gen_predicate(rng, depth - 1)],
        },
        1 => FieldPredicate::AllOf {
            all_of: vec![gen_predicate(rng, depth - 1), gen_predicate(rng, depth - 1)],
        },
        _ => FieldPredicate::Not {
            not: Box::new(gen_predicate(rng, depth - 1)),
        },
    }
}

struct Fields(serde_json::Value);

impl Env for Fields {
    fn status(&self) -> Val<'_> {
        Val::Null
    }
    fn var(&self, name: &str) -> Val<'_> {
        self.0.get(name).map_or(Val::Null, Val::json)
    }
    fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
        None
    }
}

#[test]
fn lowered_field_predicates_match_the_old_evaluator() {
    let values = [
        serde_json::Value::Null,
        serde_json::json!("a"),
        serde_json::json!(""),
        serde_json::json!(true),
        serde_json::json!(false),
        serde_json::json!(1),
        serde_json::json!([]),
        serde_json::json!(["a"]),
    ];
    let mut rng = Rng(0xf1e1d);
    for case in 0..20_000 {
        let predicate = gen_predicate(&mut rng, 3);
        let expr = field_predicate_to_expr(&predicate).unwrap();
        for _ in 0..4 {
            let mut object = serde_json::Map::new();
            for field in ["x", "y"] {
                if rng.next(4) > 0 {
                    object.insert(field.into(), rng.pick(&values).clone());
                }
            }
            let fields = serde_json::Value::Object(object);
            let old = predicate.evaluate(&fields);
            assert_eq!(
                eval(&expr, &Fields(fields.clone())),
                if old { Truth::True } else { Truth::False },
                "case {case}: {predicate:?} => `{expr}` on {fields}"
            );
        }
    }
}

#[test]
fn lowers_invariant_forms() {
    let lower = |when: &[&str], assert: &str| {
        let when: Vec<String> = when.iter().map(|s| s.to_string()).collect();
        invariant_to_expr(&when, assert).unwrap()
    };
    let text = |l: LoweredInvariant| match l {
        LoweredInvariant::Assert(e) => e.to_string(),
        other => panic!("{other:?}"),
    };
    assert_eq!(text(lower(&[], "items > 0")), "items > 0");
    assert_eq!(
        text(lower(&["A", "B"], "ready")),
        "status in ['A', 'B'] => ready"
    );
    assert_eq!(
        text(lower(&[], "!ready && count <= 5")),
        "!ready && count <= 5"
    );
    assert_eq!(text(lower(&[], "never(Broken)")), "status != 'Broken'");
    assert_eq!(
        text(lower(&["A"], "is_true has_goal")),
        "status in ['A'] => has_goal"
    );
    assert_eq!(
        text(lower(&["A"], "goal != ''")),
        "status in ['A'] => goal != ''"
    );
    assert_eq!(text(lower(&[], "true")), "true");
    assert_eq!(
        lower(&["Done"], "no_further_transitions"),
        LoweredInvariant::Terminal(vec!["Done".into()])
    );
    assert!(matches!(
        lower(&["X"], "ordering(A, B)"),
        LoweredInvariant::Dropped(_)
    ));
    let _: Expr = crate::predicate::parse("true").unwrap();
}

const OLD_SPEC: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Review", "Done", "Gone"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[state]]
name = "ready"
type = "bool"
initial = "false"

[[state]]
name = "title"
type = "string"
initial = ""

# Submit needs content.
[[action]]
name = "Submit"
from = ["Draft"]
to = "Review"
guard = ["is_true ready", { type = "min_count", var = "items", min = 2 }]

[[action]]
name = "Approve"
from = ["Review"]
to = "Done"
guard = [{ type = "cross_entity_state", entity_type = "Folder", entity_id_source = "folder_id", forbidden_status = ["Frozen"] }]

[[action.triggers]]
name = "notify"
kind = "entity"
target_entity = "Folder"
target_action = "Touch"
guard = { type = "all_of", guards = [{ type = "bool_true", field = "ready" }, { type = "state_in", values = ["Done"] }] }
[action.triggers.resolve_target]
type = "field"
field = "folder_id"

[[invariant]]
name = "ReviewHasItems"
when = ["Review", "Done"]
assert = "items > 0"

[[invariant]]
name = "DoneIsFinal"
when = ["Done", "Gone"]
assert = "no_further_transitions"

[[invariant]]
name = "Ordered"
when = ["Done"]
assert = "ordering(Review, Done)"

[[invariant]]
name = "DoneHasTitle"
when = ["Done"]
assert = "title != ''"

[[field_invariant]]
name = "KindKnown"
when = { not = { field = "Kind", absent = true } }
require = { any_of = [{ field = "Kind", equals = "a" }, { field = "Kind", equals = "b" }] }
"#;

#[test]
fn converts_every_old_form_and_keeps_comments() {
    let migration = super::migrate_source(OLD_SPEC).expect("converts");
    let out = &migration.source;
    for expected in [
        "# Submit needs content.",
        r#"guard = "ready && items >= 2""#,
        r#"guard = "Folder[folder_id].status not in ['Frozen']""#,
        r#"guard = "ready && status in ['Done']""#,
        r#"assert = "status in ['Review', 'Done'] => items > 0""#,
        r#"terminal = ["Done", "Gone"]"#,
        r#"assert = "status in ['Done'] => title != ''""#,
        r#"assert = "Kind != null => Kind in ['a', 'b']""#,
    ] {
        assert!(out.contains(expected), "missing `{expected}` in:\n{out}");
    }
    for gone in ["when =", "require =", "no_further_transitions", "ordering("] {
        assert!(!out.contains(gone), "`{gone}` survived in:\n{out}");
    }
    // The string check became a field invariant, and the history predicate
    // was dropped; both are reported.
    let automaton = crate::automaton::parse_automaton_with_liveness(
        out,
        crate::automaton::LivenessEnforcement::WarnOnly,
    )
    .expect("converted spec loads");
    assert_eq!(
        automaton.invariants.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        vec!["ReviewHasItems"]
    );
    assert!(automaton.field_invariants.iter().any(|f| f.name == "DoneHasTitle"));
    assert_eq!(migration.notes.len(), 2, "{:?}", migration.notes);
    // Converting again changes nothing.
    assert_eq!(super::migrate_source(out).unwrap().source, *out);
}

#[test]
fn an_invariant_over_an_undeclared_name_is_not_moved() {
    let spec = OLD_SPEC.replace("assert = \"title != ''\"", "assert = \"titel != ''\"");
    let err = super::migrate_source(&spec).expect_err("typo must surface");
    assert!(err.contains("unknown state variable 'titel'"), "{err}");
}
