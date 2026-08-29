//! Cross-entity collection workflow linting.

use std::collections::{BTreeMap, BTreeSet};

use super::super::{Automaton, Effect, TriggerKind};
use super::BundleLintFinding;

pub(super) fn lint_workflows(
    automata: &BTreeMap<String, Automaton>,
    entity_name: &str,
    automaton: &Automaton,
    findings: &mut Vec<BundleLintFinding>,
) {
    for workflow in &automaton.collection_workflows {
        let Some(member) = automata.get(&workflow.member_entity) else {
            findings.push(BundleLintFinding::error(
                entity_name,
                "collection_member_entity_missing",
                format!(
                    "collection_workflow '{}' references unknown member entity '{}'",
                    workflow.name, workflow.member_entity
                ),
            ));
            continue;
        };
        if member.keys.iter().any(|key| key.entity_id) {
            findings.push(BundleLintFinding::error(
                entity_name,
                "collection_member_entity_id_key_forbidden",
                format!(
                    "collection_workflow '{}' member entity '{}' declares entity_id = true",
                    workflow.name, workflow.member_entity
                ),
            ));
        }
        let member_params = [
            ("workflow_id", "string"),
            ("member_id", "string"),
            ("member_value", "string"),
            ("source_entity_id", "string"),
            ("member_index", "int"),
        ];
        let cancel_params = [
            ("workflow_id", "string"),
            ("member_id", "string"),
            ("member_value", "string"),
            ("source_entity_id", "string"),
            ("member_index", "int"),
            ("requested_outcome", "string"),
        ];
        for (role, action_name, expected) in [
            (
                "member_action",
                workflow.member_action.as_str(),
                member_params.as_slice(),
            ),
            (
                "member_cancel_action",
                workflow.member_cancel_action.as_str(),
                cancel_params.as_slice(),
            ),
        ] {
            let Some(action) = member
                .actions
                .iter()
                .find(|action| action.name == action_name)
            else {
                findings.push(BundleLintFinding::error(
                    entity_name,
                    "collection_member_action_missing",
                    format!(
                        "collection_workflow '{}' {role} references missing '{}.{}'",
                        workflow.name, workflow.member_entity, action_name
                    ),
                ));
                continue;
            };
            if role == "member_action"
                && !action.from.is_empty()
                && !action
                    .from
                    .iter()
                    .any(|state| state == &member.automaton.initial)
            {
                findings.push(BundleLintFinding::error(
                    entity_name,
                    "collection_member_action_not_initial",
                    format!(
                        "collection_workflow '{}' member action '{}.{}' is not enabled from initial state '{}'",
                        workflow.name, workflow.member_entity, action_name, member.automaton.initial
                    ),
                ));
            }
            if !super::super::collection_contract::params_match(action, expected) {
                findings.push(BundleLintFinding::error(
                    entity_name,
                    "collection_member_params_invalid",
                    format!(
                        "collection_workflow '{}' {role} '{}.{}' has invalid reserved parameters",
                        workflow.name, workflow.member_entity, action_name
                    ),
                ));
            }
            if role == "member_action" {
                lint_member_integration(entity_name, workflow, member, action, findings);
            }
        }
    }
}

fn lint_member_integration(
    source_entity: &str,
    workflow: &super::super::CollectionWorkflow,
    member: &Automaton,
    action: &super::super::Action,
    findings: &mut Vec<BundleLintFinding>,
) {
    let effects = action
        .effect
        .iter()
        .filter_map(|effect| match effect {
            Effect::Trigger { name } => Some(name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if effects.len() != 1 {
        findings.push(BundleLintFinding::error(
            source_entity,
            "collection_member_integration_count",
            format!(
                "collection_workflow '{}' member action '{}.{}' must trigger exactly one integration",
                workflow.name, workflow.member_entity, workflow.member_action
            ),
        ));
        return;
    }
    let effect_name = effects[0];
    let matching = member
        .integrations
        .iter()
        .filter(|integration| integration.trigger == effect_name)
        .collect::<Vec<_>>();
    if matching.len() != 1 || matching[0].integration_type != "wasm" {
        findings.push(BundleLintFinding::error(
            source_entity,
            "collection_member_integration_not_wasm",
            format!(
                "collection_workflow '{}' member action '{}.{}' effect '{}' must resolve uniquely to WASM",
                workflow.name, workflow.member_entity, workflow.member_action, effect_name
            ),
        ));
        return;
    }
    let integration = matching[0];
    let Some(success) = integration.on_success.as_deref() else {
        findings.push(BundleLintFinding::error(
            source_entity,
            "collection_member_success_callback_missing",
            format!(
                "collection_workflow '{}' member WASM integration '{}' requires static on_success",
                workflow.name, integration.name
            ),
        ));
        return;
    };
    let callbacks = std::iter::once(success)
        .chain(integration.on_failure.as_deref())
        .chain(
            integration
                .failure_routes
                .iter()
                .map(|route| route.callback_action.as_str()),
        )
        .collect::<BTreeSet<_>>();
    for callback in callbacks {
        let Some(callback_action) = member.actions.iter().find(|action| action.name == callback)
        else {
            continue;
        };
        if callback_action
            .effect
            .iter()
            .any(|effect| matches!(effect, Effect::Trigger { .. }))
        {
            findings.push(BundleLintFinding::error(
                source_entity,
                "collection_member_callback_integration_forbidden",
                format!(
                    "collection_workflow '{}' callback '{}.{}' cannot trigger another integration",
                    workflow.name, workflow.member_entity, callback
                ),
            ));
        }
    }
}

pub(super) fn lint_role_uniqueness(
    automata: &BTreeMap<String, Automaton>,
    findings: &mut Vec<BundleLintFinding>,
) {
    let mut roles = BTreeMap::<(String, String), (String, String)>::new();
    for (source_entity, automaton) in automata {
        for workflow in &automaton.collection_workflows {
            let assignments = [
                (
                    source_entity.as_str(),
                    workflow.start_action.as_str(),
                    "start",
                ),
                (
                    source_entity.as_str(),
                    workflow.cancel_action.as_str(),
                    "cancel",
                ),
                (
                    source_entity.as_str(),
                    workflow.timeout_action.as_str(),
                    "timeout",
                ),
                (source_entity.as_str(), workflow.on_success.as_str(), "join"),
                (
                    source_entity.as_str(),
                    workflow.on_partial_failure.as_str(),
                    "join",
                ),
                (source_entity.as_str(), workflow.on_failure.as_str(), "join"),
                (
                    source_entity.as_str(),
                    workflow.on_cancelled.as_str(),
                    "join",
                ),
                (
                    source_entity.as_str(),
                    workflow.on_timed_out.as_str(),
                    "join",
                ),
                (
                    workflow.member_entity.as_str(),
                    workflow.member_action.as_str(),
                    "member",
                ),
                (
                    workflow.member_entity.as_str(),
                    workflow.member_cancel_action.as_str(),
                    "member_cancel",
                ),
            ];
            for (entity, action, role) in assignments {
                let owner = (source_entity.clone(), workflow.name.clone());
                if let Some((previous_entity, previous_workflow)) =
                    roles.insert((entity.to_string(), action.to_string()), owner)
                {
                    findings.push(BundleLintFinding::error(
                        source_entity,
                        "collection_action_role_alias",
                        format!(
                            "collection role {entity}.{action} ({role}) aliases workflow '{}.{}'",
                            previous_entity, previous_workflow
                        ),
                    ));
                }
            }
        }
    }
    for (source_entity, automaton) in automata {
        for workflow in &automaton.collection_workflows {
            let Some(member) = automata.get(&workflow.member_entity) else {
                continue;
            };
            let Some(member_action) = member
                .actions
                .iter()
                .find(|action| action.name == workflow.member_action)
            else {
                continue;
            };
            let effects = member_action
                .effect
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Trigger { name } => Some(name.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if effects.len() != 1 {
                continue;
            }
            let integrations = member
                .integrations
                .iter()
                .filter(|integration| integration.trigger == effects[0])
                .collect::<Vec<_>>();
            if integrations.len() != 1 {
                continue;
            }
            let integration = integrations[0];
            let callbacks = integration
                .on_success
                .as_deref()
                .into_iter()
                .chain(integration.on_failure.as_deref())
                .chain(
                    integration
                        .failure_routes
                        .iter()
                        .map(|route| route.callback_action.as_str()),
                )
                .collect::<BTreeSet<_>>();
            for callback in callbacks {
                if roles.contains_key(&(workflow.member_entity.clone(), callback.to_string())) {
                    findings.push(BundleLintFinding::error(
                        source_entity,
                        "collection_member_callback_role_alias",
                        format!(
                            "collection_workflow '{}' callback '{}.{}' cannot also be a collection role action",
                            workflow.name, workflow.member_entity, callback
                        ),
                    ));
                }
            }
        }
    }
    for (entity, automaton) in automata {
        for action in &automaton.actions {
            if !roles.contains_key(&(entity.clone(), action.name.clone())) {
                continue;
            }
            for trigger in &action.triggers {
                let mut targets = Vec::new();
                if trigger.kind == TriggerKind::Entity
                    && let (Some(target_entity), Some(target_action)) =
                        (&trigger.target_entity, &trigger.target_action)
                {
                    targets.push((target_entity, target_action));
                }
                if let Some(target_action) = &trigger.on_success {
                    targets.push((entity, target_action));
                }
                if let Some(target_action) = &trigger.on_failure {
                    targets.push((entity, target_action));
                }
                for (target_entity, target_action) in targets {
                    if roles.contains_key(&(target_entity.clone(), target_action.clone())) {
                        findings.push(BundleLintFinding::error(
                            entity,
                            "collection_action_role_recursion",
                            format!(
                                "collection role {}.{} triggers collection role {}.{}",
                                entity, action.name, target_entity, target_action
                            ),
                        ));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
}
