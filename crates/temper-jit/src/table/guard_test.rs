//! Tests for guard evaluation (`check`, `check_detailed`).

use super::*;
use serde_json::json;

fn guard(source: &str) -> Expr {
    temper_spec::predicate::parse(source).unwrap()
}

fn declared() -> InitialValues {
    let mut declared = InitialValues::default();
    declared.counters.insert("items".into(), 0);
    declared.booleans.insert("ready".into(), false);
    declared.lists.insert("tags".into(), vec![]);
    declared
}

fn holds(source: &str, state: &str, ctx: &EvalContext) -> bool {
    let declared = declared();
    let expr = guard(source);
    let result = check(&expr, state, ctx, &declared);
    // The cold path agrees with the hot path.
    assert_eq!(
        check_detailed(&expr, state, ctx, &declared).is_none(),
        result,
        "check and check_detailed disagree on `{source}`"
    );
    result
}

#[test]
fn reads_counters_booleans_lists_and_status() {
    let mut ctx = EvalContext::default();
    ctx.counters.insert("items".into(), 3);
    ctx.booleans.insert("ready".into(), true);
    ctx.lists.insert("tags".into(), vec!["urgent".into()]);
    assert!(holds("true", "Draft", &ctx));
    assert!(holds("status in ['Draft', 'Open']", "Draft", &ctx));
    assert!(!holds("status == 'Open'", "Draft", &ctx));
    assert!(holds("items >= 3 && items < 4", "Draft", &ctx));
    assert!(holds(
        "ready && 'urgent' in tags && len(tags) == 1",
        "Draft",
        &ctx
    ));
    assert!(!holds("!ready || items > 10", "Draft", &ctx));
}

#[test]
fn declared_variables_missing_from_the_context_read_as_zero_values() {
    let ctx = EvalContext::default();
    assert!(holds("items == 0 && items < 1", "A", &ctx));
    assert!(holds("!ready", "A", &ctx));
    assert!(holds("len(tags) == 0 && 'x' not in tags", "A", &ctx));
    // Undeclared names are absent.
    assert!(holds("empty(parent_id)", "A", &ctx));
}

#[test]
fn reads_fields() {
    let mut ctx = EvalContext::default();
    ctx.fields
        .insert("goal".into(), serde_json::json!("ship it"));
    assert!(holds("goal != '' && !empty(goal)", "A", &ctx));
    assert!(!holds("goal == null", "A", &ctx));
}

#[test]
fn related_statuses_must_hold_for_every_entity_and_missing_reads_null() {
    let key = ("Workspace".to_string(), "workspace_id".to_string());
    let allow = "Workspace[workspace_id].status in ['Active']";
    let deny = "Workspace[workspace_id].status not in ['Frozen']";
    let mut ctx = EvalContext::default();
    // Unset reference.
    assert!(!holds(allow, "A", &ctx));
    assert!(holds(deny, "A", &ctx));
    ctx.related
        .insert(key.clone(), Related::Statuses(vec![Some("Active".into())]));
    assert!(holds(allow, "A", &ctx));
    ctx.related.insert(
        key.clone(),
        Related::Statuses(vec![Some("Active".into()), Some("Frozen".into())]),
    );
    assert!(!holds(allow, "A", &ctx));
    assert!(!holds(deny, "A", &ctx));
    // A related entity that is not found.
    ctx.related.insert(key, Related::Statuses(vec![None]));
    assert!(!holds(allow, "A", &ctx));
    assert!(holds(deny, "A", &ctx));
}

#[test]
fn failure_names_the_failing_conjunct_and_its_values() {
    let mut ctx = EvalContext::default();
    ctx.counters.insert("items".into(), 1);
    ctx.booleans.insert("ready".into(), true);
    let failure = check_detailed(&guard("ready && items >= 2"), "Draft", &ctx, &declared())
        .expect("guard fails");
    assert_eq!(
        failure,
        GuardFailure {
            expr: "items >= 2".into(),
            found: vec![("items".into(), "1".into())],
            conditions: vec![condition("items", ">=", json!(2), json!(1))],
        }
    );
    let failure = check_detailed(
        &guard("Workspace[workspace_id].status in ['Active']"),
        "Draft",
        &ctx,
        &declared(),
    )
    .expect("guard fails");
    assert_eq!(
        failure.found,
        vec![
            ("workspace_id".into(), "<missing>".into()),
            ("Workspace[workspace_id].status".into(), "<missing>".into()),
        ]
    );
}

fn condition(
    field: &str,
    op: &str,
    wants: serde_json::Value,
    value: serde_json::Value,
) -> GuardCondition {
    GuardCondition {
        field: field.into(),
        op: op.into(),
        wants,
        value,
    }
}

fn conditions(source: &str, state: &str, ctx: &EvalContext) -> (String, Vec<GuardCondition>) {
    let failure = check_detailed(&guard(source), state, ctx, &declared()).expect("guard fails");
    (failure.expr, failure.conditions)
}

#[test]
fn failure_splits_field_comparisons_into_typed_conditions() {
    let mut ctx = EvalContext::default();
    ctx.fields
        .insert("branch".into(), json!("Market Street, x = 1"));
    ctx.counters.insert("holders".into(), 2);
    ctx.booleans.insert("waiting".into(), true);
    assert_eq!(
        conditions("branch == 'Harbour'", "Open", &ctx),
        (
            "branch == 'Harbour'".into(),
            vec![condition(
                "branch",
                "==",
                json!("Harbour"),
                json!("Market Street, x = 1")
            )]
        )
    );
    assert_eq!(
        conditions("holders <= 1", "Open", &ctx).1,
        vec![condition("holders", "<=", json!(1), json!(2))]
    );
    // A literal on the left reads from the field's side.
    assert_eq!(
        conditions("1 >= holders", "Open", &ctx).1,
        vec![condition("holders", "<=", json!(1), json!(2))]
    );
    assert_eq!(
        conditions("!waiting", "Open", &ctx).1,
        vec![condition("waiting", "==", json!(false), json!(true))]
    );
    // A declared boolean missing from the context reads as false.
    assert_eq!(
        conditions("ready", "Open", &ctx).1,
        vec![condition("ready", "==", json!(true), json!(false))]
    );
    assert_eq!(
        conditions("status in ['Draft', 'Review']", "Open", &ctx).1,
        vec![condition(
            "status",
            "in",
            json!(["Draft", "Review"]),
            json!("Open")
        )]
    );
    // An absent field reads as null.
    assert_eq!(
        conditions("owner == 'sesh'", "Open", &ctx).1,
        vec![condition("owner", "==", json!("sesh"), json!(null))]
    );
}

#[test]
fn failure_does_not_split_what_has_no_single_field_and_wanted_value() {
    let mut ctx = EvalContext::default();
    ctx.counters.insert("holders".into(), 2);
    ctx.counters.insert("items".into(), 0);
    ctx.booleans.insert("waiting".into(), true);
    for source in [
        "holders <= 1 || !waiting",
        "len(tags) > 0",
        "!empty(owner)",
        "holders == items",
        "Workspace[workspace_id].status == 'Active'",
        "!(holders >= 1)",
        "waiting => holders <= 1",
    ] {
        let (expr, split) = conditions(source, "Open", &ctx);
        assert!(split.is_empty(), "`{source}` split into {split:?}");
        assert!(!expr.is_empty());
    }
    // The failing guard is the whole `||`, as the message names it.
    assert_eq!(
        conditions("holders <= 1 || !waiting", "Open", &ctx).0,
        "holders <= 1 || !waiting"
    );
}

#[test]
fn failure_conditions_preserve_stored_numbers() {
    let mut ctx = EvalContext::default();
    for value in [
        json!(9_007_199_254_740_993_u64),
        json!(-9_007_199_254_740_993_i64),
        json!(u64::MAX),
        json!(1.5),
    ] {
        ctx.fields.insert("revision".into(), value.clone());
        for source in ["revision == 0", "0 == revision", "revision in [0]"] {
            assert_eq!(conditions(source, "Open", &ctx).1[0].value, value);
        }
    }
    // Counters take precedence over fields with the same name.
    let counter = usize::MAX;
    ctx.counters.insert("revision".into(), counter);
    assert_eq!(
        conditions("revision == 0", "Open", &ctx).1[0].value,
        json!(counter)
    );
}
