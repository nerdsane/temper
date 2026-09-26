//! Tests for guard evaluation (`check`, `check_detailed`).

use super::*;

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
    // Unresolved target.
    ctx.related
        .insert(key.clone(), Related::Statuses(vec![None]));
    assert!(!holds(allow, "A", &ctx));
    assert!(holds(deny, "A", &ctx));
    // Unresolved (budget exhausted): neither passes.
    ctx.related.insert(key, Related::Unresolved);
    assert!(!holds(allow, "A", &ctx));
    assert!(!holds(deny, "A", &ctx));
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
