use std::collections::BTreeMap;

use super::*;

fn p(source: &str) -> Expr {
    parse(source).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn precedence_is_implies_or_and_not() {
    assert_eq!(
        p("a || b && !c => d"),
        Expr::Implies(
            Box::new(Expr::Or(vec![
                Expr::Var("a".into()),
                Expr::And(vec![
                    Expr::Var("b".into()),
                    Expr::Not(Box::new(Expr::Var("c".into())))
                ]),
            ])),
            Box::new(Expr::Var("d".into())),
        )
    );
    // Implication is right-associative.
    assert_eq!(p("a => b => c"), p("a => (b => c)"));
    assert_ne!(p("a => b => c"), p("(a => b) => c"));
}

#[test]
fn parses_every_operand_form() {
    let cases = [
        "status == 'Draft'",
        "status in ['Draft', 'Open']",
        "status not in []",
        "items >= 2",
        "items < limit",
        "len(tags) > 0",
        "'urgent' in tags",
        "Workspace[workspace_id].status not in ['Frozen']",
        "kind == 'external' => !empty(url)",
        "url == null",
        "retries == -1",
        "true",
        "flag == false",
    ];
    for source in cases {
        let expr = p(source);
        assert_eq!(expr.to_string(), source, "canonical form of {source}");
    }
}

#[test]
fn printing_round_trips_through_the_parser() {
    // Deterministic generator: covers nesting the fixed cases above do not.
    let mut seed: u64 = 0x5eed;
    let mut next = move |bound: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % bound
    };
    fn gen_expr(next: &mut dyn FnMut(u64) -> u64, depth: u32) -> Expr {
        let leaf = depth == 0 || next(3) == 0;
        if leaf {
            return match next(5) {
                0 => Expr::Var(format!("v{}", next(3))),
                1 => Expr::Empty("s".into()),
                2 => Expr::Compare {
                    lhs: Operand::Var("n".into()),
                    op: [
                        CmpOp::Eq,
                        CmpOp::Ne,
                        CmpOp::Lt,
                        CmpOp::Le,
                        CmpOp::Gt,
                        CmpOp::Ge,
                    ][next(6) as usize],
                    rhs: Operand::Lit(Literal::Int(next(10) as i64 - 3)),
                },
                3 => Expr::In {
                    value: Operand::Status,
                    set: Set::List(vec![Literal::Str("A".into()), Literal::Str("B".into())]),
                    negated: next(2) == 0,
                },
                _ => Expr::Const(next(2) == 0),
            };
        }
        let d = depth - 1;
        match next(4) {
            0 => Expr::Not(Box::new(gen_expr(next, d))),
            1 => Expr::And(vec![gen_expr(next, d), gen_expr(next, d)]),
            2 => Expr::Or(vec![gen_expr(next, d), gen_expr(next, d)]),
            _ => Expr::Implies(Box::new(gen_expr(next, d)), Box::new(gen_expr(next, d))),
        }
    }
    fn normalize(expr: Expr) -> Expr {
        // The parser flattens `a && (b && c)`; mirror that before comparing.
        match expr {
            Expr::And(parts) => {
                let mut flat = Vec::new();
                for part in parts.into_iter().map(normalize) {
                    match part {
                        Expr::And(inner) => flat.extend(inner),
                        other => flat.push(other),
                    }
                }
                Expr::And(flat)
            }
            Expr::Or(parts) => {
                let mut flat = Vec::new();
                for part in parts.into_iter().map(normalize) {
                    match part {
                        Expr::Or(inner) => flat.extend(inner),
                        other => flat.push(other),
                    }
                }
                Expr::Or(flat)
            }
            Expr::Not(inner) => Expr::Not(Box::new(normalize(*inner))),
            Expr::Implies(a, b) => Expr::Implies(Box::new(normalize(*a)), Box::new(normalize(*b))),
            other => other,
        }
    }
    for _ in 0..2000 {
        let expr = normalize(gen_expr(&mut next, 4));
        let printed = expr.to_string();
        assert_eq!(p(&printed), expr, "round trip of {printed}");
    }
}

#[test]
fn rejects_malformed_input() {
    for source in [
        "",
        "a &&",
        "status == \"Draft\"",
        "items >",
        "status",
        "len(items",
        "a b",
        "Parent[pid].name == 'x'",
        "empty == 1",
        "x not 1",
        "'unterminated",
    ] {
        assert!(parse(source).is_err(), "should reject {source:?}");
    }
    let deep = format!(
        "{}a{}",
        "(".repeat(MAX_DEPTH + 1),
        ")".repeat(MAX_DEPTH + 1)
    );
    assert!(parse(&deep).is_err());
}

struct TestEnv {
    status: &'static str,
    fields: serde_json::Value,
    cross: Option<Vec<Option<&'static str>>>,
}

impl Env for TestEnv {
    fn status(&self) -> Val<'_> {
        Val::Str(self.status)
    }
    fn var(&self, name: &str) -> Val<'_> {
        self.fields.get(name).map_or(Val::Null, Val::json)
    }
    fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
        self.cross.as_ref().map(|statuses| {
            statuses
                .iter()
                .map(|s| s.map_or(Val::Null, Val::Str))
                .collect()
        })
    }
}

fn env(fields: serde_json::Value) -> TestEnv {
    TestEnv {
        status: "Open",
        fields,
        cross: Some(vec![Some("Active")]),
    }
}

fn holds(source: &str, env: &TestEnv) -> Truth {
    eval(&p(source), env)
}

#[test]
fn evaluates_values_and_absence() {
    let e = env(serde_json::json!({
        "items": 3, "ready": true, "tags": ["a", "b"], "name": "", "url": null
    }));
    assert_eq!(holds("items >= 3 && ready", &e), Truth::True);
    assert_eq!(holds("status in ['Draft', 'Open']", &e), Truth::True);
    assert_eq!(holds("'b' in tags && len(tags) == 2", &e), Truth::True);
    assert_eq!(
        holds("empty(name) && empty(url) && empty(missing)", &e),
        Truth::True
    );
    assert_eq!(holds("url == null && missing == null", &e), Truth::True);
    // A missing bool is not true, and `== false` requires an explicit false.
    assert_eq!(holds("missing", &e), Truth::False);
    assert_eq!(holds("missing == false", &e), Truth::False);
    assert_eq!(holds("!missing", &e), Truth::True);
    // Ordering against non-numbers is false, not an error.
    assert_eq!(holds("name > 1", &e), Truth::False);
    assert_eq!(holds("ready => items > 5", &e), Truth::False);
}

#[test]
fn related_status_must_hold_for_every_entity() {
    let mut e = env(serde_json::json!({}));
    e.cross = Some(vec![Some("Active"), Some("Frozen")]);
    assert_eq!(holds("W[w].status in ['Active']", &e), Truth::False);
    assert_eq!(holds("W[w].status not in ['Archived']", &e), Truth::True);
    // Unset or unresolved reads as null.
    e.cross = Some(vec![None]);
    assert_eq!(holds("W[w].status in ['Active']", &e), Truth::False);
    assert_eq!(holds("W[w].status not in ['Frozen']", &e), Truth::True);
    assert_eq!(holds("W[w].status != null", &e), Truth::False);
}

#[test]
fn unknown_values_follow_kleene_logic() {
    let mut e = env(serde_json::json!({ "ready": true }));
    e.cross = None;
    let unknown = "W[w].status == 'Active'";
    assert_eq!(holds(unknown, &e), Truth::Unknown);
    assert_eq!(holds(&format!("!({unknown})"), &e), Truth::Unknown);
    assert_eq!(holds(&format!("{unknown} || ready"), &e), Truth::True);
    assert_eq!(holds(&format!("{unknown} && !ready"), &e), Truth::False);
    assert_eq!(holds(&format!("!ready => {unknown}"), &e), Truth::True);
    assert!(holds(unknown, &e).may_hold());
    assert!(!holds(unknown, &e).must_hold());
}

#[test]
fn explain_names_the_failing_conjunct() {
    let e = env(serde_json::json!({ "items": 1, "ready": true }));
    let expr = p("ready && items >= 2 && status == 'Open'");
    assert_eq!(
        explain(&expr, &e).map(ToString::to_string),
        Some("items >= 2".into())
    );
    assert!(explain(&p("ready"), &e).is_none());
}

#[test]
fn checks_names_and_types_in_state_scope() {
    let vars: BTreeMap<String, VarKind> = [
        ("items", VarKind::Counter),
        ("ready", VarKind::Bool),
        ("tags", VarKind::List),
        ("goal", VarKind::Str),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let scope = Scope::State(&vars);
    for ok in [
        "items >= 2 && ready",
        "'x' in tags && len(tags) < 3",
        "empty(goal) || goal == 'x'",
        "status in ['A'] => items > 0",
        "P[goal].status == 'Done'",
        "empty(parent_id) || P[parent_id].status == 'Done'",
    ] {
        check(&p(ok), scope).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    for bad in [
        "unknown_var",
        "items",
        "ready > 1",
        "items == 'three'",
        "goal in tags_missing",
        "len(items) > 0",
        "status > 1",
    ] {
        assert!(check(&p(bad), scope).is_err(), "should reject {bad}");
    }
    // Field scope accepts any name.
    check(&p("anything == 'x' && flag"), Scope::Fields).unwrap();
}

#[test]
fn reports_what_the_verifier_cannot_model() {
    let vars: BTreeMap<String, VarKind> = [
        ("items", VarKind::Counter),
        ("ready", VarKind::Bool),
        ("tags", VarKind::List),
        ("goal", VarKind::Str),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for modeled in [
        "status in ['A'] => items > 0 && ready",
        "'x' in tags || len(tags) >= 2",
        "!ready || empty(tags)",
    ] {
        assert_eq!(unmodelable(&p(modeled), &vars), None, "{modeled}");
    }
    for (source, culprit) in [
        ("ready && goal != ''", "goal != ''"),
        ("P[goal].status == 'Done'", "P[goal].status == 'Done'"),
        ("empty(goal)", "empty(goal)"),
    ] {
        assert_eq!(
            unmodelable(&p(source), &vars).map(ToString::to_string),
            Some(culprit.to_string())
        );
    }
}

#[test]
fn serializes_as_source_text() {
    let expr = p("status == 'A' && items > 0");
    let json = serde_json::to_string(&expr).unwrap();
    assert_eq!(json, "\"status == 'A' && items > 0\"");
    assert_eq!(serde_json::from_str::<Expr>(&json).unwrap(), expr);
    assert_eq!(
        p("W[a].status == 'X' || V[b].status == 'Y' || W[a].status == 'Z'").cross_refs(),
        vec![("W".into(), "a".into()), ("V".into(), "b".into())]
    );
}
