use super::*;
use temper_spec::automaton::parse_automaton;

/// The shape that motivates the whole module: an invariant over a *field*.
///
/// `root_cause` is written by a WASM module, so no declared effect produces it
/// and the cascade proves `AnalyzedHasCause` over a state space in which the
/// field is forever its declared default. Only live data can falsify it.
const SPEC: &str = r#"
[automaton]
name = "Insight"
states = ["Pending", "Analyzing", "Analyzed", "Failed"]
initial = "Pending"
terminal = ["Failed"]
allow_indefinite_states = ["Pending", "Analyzed"]

[[state]]
name = "root_cause"
type = "string"
initial = ""

[[state]]
name = "observations"
type = "counter"
initial = 0

[[action]]
name = "Observe"
kind = "input"
from = ["Pending"]
effect = ["observations += 1"]

[[action]]
name = "Analyze"
kind = "input"
from = ["Pending"]
to = "Analyzing"
guard = "observations > 0"

[[action]]
name = "Succeed"
kind = "input"
from = ["Analyzing"]
to = "Analyzed"

[[field_invariant]]
name = "AnalyzedHasCause"
assert = "status in ['Analyzed'] => !empty(root_cause)"
message = "an analysed incident must carry a root cause"

[[invariant]]
name = "AnalyzingWasObserved"
assert = "status in ['Analyzing', 'Analyzed'] => observations > 0"
"#;

fn spec() -> temper_spec::automaton::Automaton {
    parse_automaton(SPEC).expect("fixture parses")
}

/// A genuinely healthy `Analyzed` entity: a root cause *and* the observation
/// it was drawn from. Both invariants bind here, and a fixture that satisfies
/// only one is not a passing case, it is a second violation.
fn analyzed(id: &str) -> EntitySnapshot {
    let mut entity = snapshot(id, "Analyzed");
    entity
        .fields
        .insert("root_cause".to_string(), serde_json::json!("bad deploy"));
    entity.counters.insert("observations".to_string(), 2);
    entity
}

fn snapshot(id: &str, status: &str) -> EntitySnapshot {
    EntitySnapshot {
        entity_id: id.to_string(),
        status: status.to_string(),
        ..Default::default()
    }
}

#[test]
fn a_violated_field_invariant_is_reported_with_the_failing_expression() {
    // Analyzed with an empty root_cause: exactly what the cascade cannot see,
    // because nothing in the model ever writes the field.
    let mut entity = snapshot("inc-1", "Analyzed");
    entity
        .fields
        .insert("root_cause".to_string(), serde_json::json!(""));

    let findings = audit_entity(&spec(), &entity);
    let violation = findings
        .iter()
        .find(|f| f.code == "field_invariant_violated")
        .unwrap_or_else(|| panic!("expected a violation, got {findings:#?}"));

    assert_eq!(violation.severity, AuditSeverity::Violation);
    assert_eq!(violation.entity_id, "inc-1");
    assert!(
        violation.message.contains("AnalyzedHasCause"),
        "must name the invariant: {}",
        violation.message
    );
    // The values read are the difference between "something is wrong" and
    // "this field is empty"; without them the reader re-queries the entity.
    assert!(
        violation.found.iter().any(|(name, _)| name == "root_cause"),
        "must report the value read: {:?}",
        violation.found
    );
}

#[test]
fn a_satisfied_invariant_is_silent() {
    let findings = audit_entity(&spec(), &analyzed("inc-2"));
    assert!(
        !findings
            .iter()
            .any(|f| f.code.ends_with("invariant_violated")),
        "{findings:#?}"
    );
}

#[test]
fn an_invariant_scoped_to_another_status_does_not_fire() {
    // Pending with no root_cause is fine; the invariant is scoped to Analyzed.
    let findings = audit_entity(&spec(), &snapshot("inc-3", "Pending"));
    assert!(
        !findings
            .iter()
            .any(|f| f.code.ends_with("invariant_violated")),
        "{findings:#?}"
    );
}

#[test]
fn an_undeclared_status_is_reported_and_suppresses_the_rest() {
    // A row left behind by a spec that renamed this state. Every other check
    // reads the status, so they would all fire as consequences of this one.
    let findings = audit_entity(&spec(), &snapshot("inc-4", "Triaging"));
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert_eq!(findings[0].code, "status_undeclared");
    assert!(findings[0].message.contains("Triaging"), "{findings:#?}");
}

#[test]
fn an_entity_with_no_enabled_action_is_stranded() {
    // Pending, zero observations: `Analyze` is guarded by `observations > 0`
    // and `Observe` has no `to`, so nothing can move this entity on. The
    // guard evaluator is what makes this visible -- reading `from` lists alone
    // would conclude two actions are available.
    let spec = spec();
    let mut entity = snapshot("inc-5", "Analyzing");
    entity.counters.insert("observations".to_string(), 0);

    // From `Analyzing`, `Succeed` is enabled, so this is not stranded.
    let findings = audit_entity(&spec, &entity);
    assert!(
        !findings.iter().any(|f| f.code == "entity_stranded"),
        "Succeed is enabled from Analyzing: {findings:#?}"
    );

    // Remove the only way out and it is.
    let no_exit = SPEC.replace(
        "from = [\"Analyzing\"]\nto = \"Analyzed\"",
        "from = [\"Analyzed\"]\nto = \"Failed\"",
    );
    let no_exit = parse_automaton(&no_exit).expect("parses");
    let findings = audit_entity(&no_exit, &entity);
    let stranded = findings
        .iter()
        .find(|f| f.code == "entity_stranded")
        .unwrap_or_else(|| panic!("expected stranded, got {findings:#?}"));
    assert_eq!(stranded.severity, AuditSeverity::Violation);
    assert!(stranded.message.contains("Analyzing"), "{findings:#?}");
}

#[test]
fn a_terminal_state_is_never_stranded_or_transient() {
    let findings = audit_entity(&spec(), &snapshot("inc-6", "Failed"));
    assert!(
        findings.is_empty(),
        "Failed is terminal; sitting there is the point: {findings:#?}"
    );
}

#[test]
fn occupying_a_transient_state_warns_without_claiming_a_violation() {
    // `Analyzing` is neither terminal nor declared indefinite. This is the
    // entity-stuck-in-Analyzing case: a callback that never arrived.
    let findings = audit_entity(&spec(), &snapshot("inc-7", "Analyzing"));
    let warning = findings
        .iter()
        .find(|f| f.code == "transient_state_occupied")
        .unwrap_or_else(|| panic!("expected a warning, got {findings:#?}"));
    assert_eq!(warning.severity, AuditSeverity::Warning);
    // It must say what could still fire, or the reader cannot tell a stuck
    // entity from a deadlocked one.
    assert!(warning.message.contains("Succeed"), "{}", warning.message);
}

#[test]
fn an_indefinite_state_does_not_warn() {
    // `Analyzed` is declared indefinite, so resting there is intended.
    let findings = audit_entity(&spec(), &analyzed("inc-8"));
    assert!(findings.is_empty(), "{findings:#?}");
}

#[test]
fn a_tdata_row_decodes_into_a_snapshot() {
    let row = serde_json::json!({
        "entity_id": "inc-9",
        "status": "Analyzed",
        "counters": {"observations": 3},
        "booleans": {},
        "lists": {},
        "fields": {"root_cause": ""},
    });
    let snapshot = EntitySnapshot::from_tdata_row(&row).expect("decodes");
    assert_eq!(snapshot.entity_id, "inc-9");
    assert_eq!(snapshot.counters.get("observations"), Some(&3));

    // And the decoded snapshot is auditable end to end.
    let report = audit_entities(&spec(), &[snapshot]);
    assert_eq!(report.entities_audited, 1);
    assert_eq!(report.entities_violating, 1);
    assert!(report.has_violations());
}

#[test]
fn a_legacy_row_without_counters_still_audits_its_fields() {
    // Catalog rows written before the projection carried counters. Auditing
    // what they do have beats refusing to look at them.
    let row = serde_json::json!({
        "entity_id": "inc-10",
        "status": "Analyzed",
        "fields": {"root_cause": ""},
    });
    let snapshot = EntitySnapshot::from_tdata_row(&row).expect("decodes");
    assert!(snapshot.counters.is_empty());
    assert!(
        audit_entity(&spec(), &snapshot)
            .iter()
            .any(|f| f.code == "field_invariant_violated")
    );
}

#[test]
fn a_row_without_a_status_is_an_error_not_a_default() {
    let row = serde_json::json!({"entity_id": "inc-11", "fields": {}});
    let error = EntitySnapshot::from_tdata_row(&row).expect_err("no status");
    assert!(error.contains("status"), "{error}");
}

#[test]
fn a_clean_set_reports_no_violations() {
    let report = audit_entities(&spec(), &[analyzed("inc-12"), snapshot("inc-13", "Failed")]);
    assert_eq!(report.entities_audited, 2);
    assert_eq!(report.entities_violating, 0);
    assert!(!report.has_violations());
    assert!(report.findings.is_empty(), "{:#?}", report.findings);
}

/// Validate against a spec nobody wrote for this module.
///
/// A check that only fires on its author's fixture has proven that the fixture
/// matches the check, not that the check matches reality. `order.ioa.toml`
/// predates the audit and its invariant was written for the cascade.
#[test]
fn a_repo_spec_invariant_is_enforced_against_live_data() {
    const ORDER: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let order = parse_automaton(ORDER).expect("repo fixture parses");

    // `SubmitRequiresItems`: a Submitted order must have items. An order that
    // reached Submitted and then had its items counter reset -- by a spec
    // change, a migration, or a direct write -- is invisible to every level of
    // the cascade, because no sequence of declared effects produces it.
    let mut broken = EntitySnapshot {
        entity_id: "order-1".to_string(),
        status: "Submitted".to_string(),
        ..Default::default()
    };
    broken.counters.insert("items".to_string(), 0);
    broken.booleans.insert("has_address".to_string(), true);

    let findings = audit_entity(&order, &broken);
    let violation = findings
        .iter()
        .find(|f| f.code == "invariant_violated")
        .unwrap_or_else(|| panic!("expected SubmitRequiresItems to fail, got {findings:#?}"));
    assert!(
        violation.message.contains("SubmitRequiresItems"),
        "{}",
        violation.message
    );
    assert!(
        violation
            .found
            .iter()
            .any(|(name, value)| name == "items" && value == "0"),
        "must report the counter it read: {:?}",
        violation.found
    );

    // The same order with an item is clean, so the check is discriminating
    // rather than always-on.
    let mut healthy = broken.clone();
    healthy.counters.insert("items".to_string(), 1);
    assert!(
        !audit_entity(&order, &healthy)
            .iter()
            .any(|f| f.severity == AuditSeverity::Violation),
        "{:#?}",
        audit_entity(&order, &healthy)
    );
}

#[test]
fn unresolved_cross_entity_guard_does_not_prove_stranding() {
    let source = SPEC.replace(
        "name = \"Succeed\"\nkind = \"input\"\nfrom = [\"Analyzing\"]\nto = \"Analyzed\"",
        "name = \"Succeed\"\nkind = \"input\"\nfrom = [\"Analyzing\"]\nto = \"Analyzed\"\nguard = \"Workspace[workspace_id].status in ['Active']\"",
    );
    let automaton = parse_automaton(&source).expect("parses");
    let mut entity = snapshot("inc-cross", "Analyzing");
    entity.counters.insert("observations".into(), 1);
    entity
        .fields
        .insert("workspace_id".into(), serde_json::json!("ws-1"));
    let findings = audit_entity(&automaton, &entity);
    assert!(
        !findings.iter().any(|f| f.code == "entity_stranded"),
        "{findings:#?}"
    );
    assert!(
        findings.iter().any(|f| f.code == "liveness_not_evaluated"),
        "{findings:#?}"
    );
}

#[test]
fn legacy_item_count_matches_runtime_counter_fallback() {
    let order =
        parse_automaton(include_str!("../../../test-fixtures/specs/order.ioa.toml")).unwrap();
    let mut row = serde_json::json!({
        "entity_id": "legacy-order", "status": "Submitted", "item_count": 2,
        "counters": {}, "booleans": {"has_address": true}, "fields": {}
    });
    let snapshot = EntitySnapshot::from_tdata_row(&row).unwrap();
    assert!(!audit_entities(&order, &[snapshot]).has_violations());
    // An explicit counter overrides the compatibility field, as in the runtime.
    row["counters"] = serde_json::json!({"items": 0});
    let snapshot = EntitySnapshot::from_tdata_row(&row).unwrap();
    assert!(audit_entities(&order, &[snapshot]).has_violations());
}
