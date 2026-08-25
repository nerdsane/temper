//! Cross-entity collection workflow linting.

use std::collections::BTreeMap;

use super::super::{Automaton, TriggerKind};
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
