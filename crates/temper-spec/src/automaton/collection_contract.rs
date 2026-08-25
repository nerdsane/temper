//! ADR-0181 collection workflow declaration validation.

use std::collections::{BTreeMap, BTreeSet};

use super::parser::AutomatonParseError;
use super::{Action, Automaton};

pub(super) fn params_match(action: &Action, expected: &[(&str, &str)]) -> bool {
    if action.params.len() != expected.len() {
        return false;
    }
    let mut names = BTreeSet::new();
    action.params.iter().all(|param| {
        names.insert(param.name())
            && expected
                .iter()
                .any(|(name, ty)| *name == param.name() && *ty == param.param_type())
    })
}

pub(super) fn validate_local(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    use super::{
        MAX_COLLECTION_WORKFLOW_ATTEMPTS, MAX_COLLECTION_WORKFLOW_CONCURRENCY,
        MAX_COLLECTION_WORKFLOW_MEMBERS,
    };
    let actions = automaton
        .actions
        .iter()
        .map(|a| (a.name.as_str(), a))
        .collect::<BTreeMap<_, _>>();
    let fields = automaton
        .state
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect::<BTreeMap<_, _>>();
    let mut names = BTreeSet::new();
    let mut roles = BTreeMap::<&str, &str>::new();
    let joins = [
        ("workflow_id", "string"),
        ("total_members", "int"),
        ("succeeded_members", "int"),
        ("failed_members", "int"),
        ("cancelled_members", "int"),
        ("timed_out_members", "int"),
    ];
    for workflow in &automaton.collection_workflows {
        if workflow.name.is_empty() || !names.insert(workflow.name.as_str()) {
            return invalid(format!(
                "collection_workflow name '{}' must be non-empty and unique",
                workflow.name
            ));
        }
        if workflow.max_members == 0
            || workflow.max_members > MAX_COLLECTION_WORKFLOW_MEMBERS
            || workflow.max_concurrency == 0
            || workflow.max_concurrency > MAX_COLLECTION_WORKFLOW_CONCURRENCY
            || u16::from(workflow.max_concurrency) > workflow.max_members
            || workflow.max_attempts == 0
            || workflow.max_attempts > MAX_COLLECTION_WORKFLOW_ATTEMPTS
        {
            return invalid(format!(
                "collection_workflow '{}' budgets must satisfy max_members=1..=64, max_concurrency=1..=8 and <= max_members, max_attempts=1..=5",
                workflow.name
            ));
        }
        let roster = fields.get(workflow.roster_field.as_str()).ok_or_else(|| {
            AutomatonParseError::Validation(format!(
                "collection_workflow '{}' references undeclared roster_field '{}'",
                workflow.name, workflow.roster_field
            ))
        })?;
        if roster.var_type != "list" {
            return invalid(format!(
                "collection_workflow '{}' roster_field '{}' must have type 'list'",
                workflow.name, workflow.roster_field
            ));
        }
        for (role, name) in source_roles(workflow) {
            if !actions.contains_key(name) {
                return invalid(format!(
                    "collection_workflow '{}' {role} references unknown action '{name}'",
                    workflow.name
                ));
            }
            if let Some(previous) = roles.insert(name, role) {
                return invalid(format!(
                    "collection workflow action '{name}' is assigned to both {previous} and {role}"
                ));
            }
        }
        if workflow.member_entity.is_empty()
            || workflow.member_action.is_empty()
            || workflow.member_cancel_action.is_empty()
            || workflow.member_action == workflow.member_cancel_action
        {
            return invalid(format!(
                "collection_workflow '{}' requires distinct non-empty member actions and a member entity",
                workflow.name
            ));
        }
        let timeouts = automaton
            .state_timeouts
            .iter()
            .filter(|t| t.on_timeout == workflow.timeout_action)
            .collect::<Vec<_>>();
        if timeouts.len() != 1 {
            return invalid(format!(
                "collection_workflow '{}' timeout_action '{}' must be owned by exactly one state_timeout",
                workflow.name, workflow.timeout_action
            ));
        }
        let timeout = timeouts[0];
        if timeout.reset_on.as_slice() != [workflow.start_action.as_str()] {
            return invalid(format!(
                "collection_workflow '{}' state_timeout reset_on must contain exactly start_action '{}'",
                workflow.name, workflow.start_action
            ));
        }
        if actions[workflow.start_action.as_str()].to.as_deref() != Some(timeout.state.as_str()) {
            return invalid(format!(
                "collection_workflow '{}' start_action must enter the bound timeout state '{}'",
                workflow.name, timeout.state
            ));
        }
        for name in join_names(workflow) {
            let action = actions[name];
            if action.from.as_slice() != [timeout.state.as_str()]
                || action.to.as_deref().is_none_or(|s| s == timeout.state)
            {
                return invalid(format!(
                    "collection_workflow '{}' join action '{}' must leave timeout state '{}'",
                    workflow.name, name, timeout.state
                ));
            }
            if !params_match(action, &joins) {
                return invalid(format!(
                    "collection_workflow '{}' join action '{}' has invalid reserved parameters",
                    workflow.name, name
                ));
            }
        }
        for action in &automaton.actions {
            let reserved = source_roles(workflow)
                .iter()
                .any(|(_, name)| *name == action.name);
            if !reserved
                && action.from.iter().any(|s| s == &timeout.state)
                && action.to.as_deref().is_some_and(|s| s != timeout.state)
            {
                return invalid(format!(
                    "collection_workflow '{}' action '{}' can leave active timeout state '{}'",
                    workflow.name, action.name, timeout.state
                ));
            }
        }
    }
    Ok(())
}

fn invalid<T>(message: String) -> Result<T, AutomatonParseError> {
    Err(AutomatonParseError::Validation(message))
}

fn source_roles(workflow: &super::CollectionWorkflow) -> [(&'static str, &str); 8] {
    [
        ("start_action", &workflow.start_action),
        ("cancel_action", &workflow.cancel_action),
        ("timeout_action", &workflow.timeout_action),
        ("on_success", &workflow.on_success),
        ("on_partial_failure", &workflow.on_partial_failure),
        ("on_failure", &workflow.on_failure),
        ("on_cancelled", &workflow.on_cancelled),
        ("on_timed_out", &workflow.on_timed_out),
    ]
}

fn join_names(workflow: &super::CollectionWorkflow) -> [&str; 5] {
    [
        &workflow.on_success,
        &workflow.on_partial_failure,
        &workflow.on_failure,
        &workflow.on_cancelled,
        &workflow.on_timed_out,
    ]
}
