use super::*;
use crate::automaton::parse_automaton;

fn workflow() -> super::super::super::CollectionWorkflow {
    super::super::super::CollectionWorkflow {
        name: "checks".to_string(),
        start_action: "StartChecks".to_string(),
        cancel_action: "CancelChecks".to_string(),
        timeout_action: "ChecksTimedOut".to_string(),
        roster_field: "members".to_string(),
        member_entity: "CheckRun".to_string(),
        member_action: "Start".to_string(),
        member_cancel_action: "Cancel".to_string(),
        max_members: 8,
        max_concurrency: 2,
        max_attempts: 3,
        on_success: "Succeeded".to_string(),
        on_partial_failure: "PartiallyFailed".to_string(),
        on_failure: "Failed".to_string(),
        on_cancelled: "Cancelled".to_string(),
        on_timed_out: "TimedOut".to_string(),
    }
}

fn member() -> Automaton {
    parse_automaton(
        r#"
[automaton]
name = "CheckRun"
states = ["Pending", "Done", "Failed"]
initial = "Pending"
allow_indefinite_states = ["Pending", "Done", "Failed"]

[[action]]
name = "Start"
from = ["Pending"]
to = "Pending"
effect = [{ type = "trigger", name = "run_check" }]

[[action]]
name = "Succeeded"
from = ["Pending"]
to = "Done"

[[action]]
name = "Failed"
from = ["Pending"]
to = "Failed"

[[integration]]
name = "check"
trigger = "run_check"
type = "wasm"
module = "check.wasm"
on_success = "Succeeded"
on_failure = "Failed"
"#,
    )
    .expect("member spec")
}

#[test]
fn member_contract_requires_exactly_one_direct_wasm_integration() {
    let mut member = member();
    let workflow = workflow();
    let mut findings = Vec::new();
    let action = member
        .actions
        .iter()
        .find(|action| action.name == "Start")
        .unwrap();
    lint_member_integration("Batch", &workflow, &member, action, &mut findings);
    assert!(findings.is_empty());

    member
        .actions
        .iter_mut()
        .find(|action| action.name == "Start")
        .unwrap()
        .effect
        .clear();
    let action = member
        .actions
        .iter()
        .find(|action| action.name == "Start")
        .unwrap();
    lint_member_integration("Batch", &workflow, &member, action, &mut findings);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "collection_member_integration_count")
    );
}

#[test]
fn member_contract_requires_static_success_and_forbids_nested_callback_integration() {
    let workflow = workflow();
    let mut missing = member();
    missing.integrations[0].on_success = None;
    let action = missing
        .actions
        .iter()
        .find(|action| action.name == "Start")
        .unwrap();
    let mut findings = Vec::new();
    lint_member_integration("Batch", &workflow, &missing, action, &mut findings);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "collection_member_success_callback_missing")
    );

    let mut nested = member();
    nested
        .actions
        .iter_mut()
        .find(|action| action.name == "Succeeded")
        .unwrap()
        .effect = vec![Effect::Trigger {
        name: "another".to_string(),
    }];
    let action = nested
        .actions
        .iter()
        .find(|action| action.name == "Start")
        .unwrap();
    findings.clear();
    lint_member_integration("Batch", &workflow, &nested, action, &mut findings);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "collection_member_callback_integration_forbidden")
    );

    let mut typed = member();
    typed.integrations[0].on_failure = None;
    typed.integrations[0].failure_routes = vec![crate::automaton::ResolvedFailureRoute {
        source_action: "Start".to_string(),
        trigger_name: "check".to_string(),
        category: temper_failure::FailureCategory::Permanent,
        callback_action: "Failed".to_string(),
    }];
    typed
        .actions
        .iter_mut()
        .find(|action| action.name == "Failed")
        .unwrap()
        .effect = vec![Effect::Trigger {
        name: "nested_typed_failure".to_string(),
    }];
    let action = typed
        .actions
        .iter()
        .find(|action| action.name == "Start")
        .unwrap();
    findings.clear();
    lint_member_integration("Batch", &workflow, &typed, action, &mut findings);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "collection_member_callback_integration_forbidden")
    );
}

#[test]
fn member_callback_cannot_alias_any_collection_role() {
    let mut source = member();
    source.automaton.name = "Batch".to_string();
    source.collection_workflows = vec![workflow()];
    let mut member = member();
    let mut nested = workflow();
    nested.name = "nested".to_string();
    nested.start_action = "Failed".to_string();
    member.collection_workflows = vec![nested];
    let automata = BTreeMap::from([
        ("Batch".to_string(), source),
        ("CheckRun".to_string(), member),
    ]);
    let mut findings = Vec::new();

    lint_role_uniqueness(&automata, &mut findings);

    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "collection_member_callback_role_alias")
    );
}
