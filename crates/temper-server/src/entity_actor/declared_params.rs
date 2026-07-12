//! Declared action-parameter boundary shared by external and internal dispatch.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use temper_jit::TransitionTable;

/// A request-body contract violation at the action boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParamContractError {
    /// A known action has no declaration metadata and cannot be filtered safely.
    MissingMetadata(String),
    /// Both spellings for one logical parameter were supplied.
    AmbiguousAlias { exact: String, alias: String },
    /// Two declared logical parameters share an accepted spelling.
    DeclarationAliasCollision {
        first: String,
        second: String,
        accepted: String,
    },
}

impl std::fmt::Display for ParamContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingMetadata(action) => write!(
                formatter,
                "Action '{action}' is missing declared-parameter metadata"
            ),
            Self::AmbiguousAlias { exact, alias } => write!(
                formatter,
                "action body supplies both '{exact}' and its canonical alias '{alias}'"
            ),
            Self::DeclarationAliasCollision {
                first,
                second,
                accepted,
            } => write!(
                formatter,
                "declared params '{first}' and '{second}' both accept spelling '{accepted}'"
            ),
        }
    }
}

/// Exact request-body keys accepted for a declared logical parameter set.
///
/// IOA specs conventionally use snake_case while their OData CSDL surface uses
/// the deterministic PascalCase spelling of the same property. Broad
/// case-folding or underscore removal is forbidden because it creates aliases
/// outside the verifier-visible contract.
fn accepted_param_keys(
    declared: &BTreeSet<String>,
) -> Result<BTreeSet<String>, ParamContractError> {
    let mut owners = BTreeMap::<String, String>::new();
    for name in declared {
        for accepted in [name.clone(), temper_spec::naming::to_pascal_case(name)] {
            if let Some(first) = owners.insert(accepted.clone(), name.clone())
                && first != *name
            {
                return Err(ParamContractError::DeclarationAliasCollision {
                    first,
                    second: name.clone(),
                    accepted,
                });
            }
        }
    }
    Ok(owners.into_keys().collect())
}

/// Restrict caller-supplied action params to the action's declared set.
///
/// Kernel-injected params survive by construction: derived values are added
/// after this filter, while spawn `copy_fields` must be declared by the target
/// initial action (the bundle lint enforces that contract).
pub(super) fn restrict_to_declared_params<'a>(
    table: &TransitionTable,
    action: &str,
    params: &'a serde_json::Value,
) -> Result<Cow<'a, serde_json::Value>, ParamContractError> {
    let Some(declared) = table.declared_params(action) else {
        return Ok(Cow::Borrowed(params));
    };
    let accepted = accepted_param_keys(declared)?;
    let Some(object) = params.as_object() else {
        return Ok(Cow::Borrowed(params));
    };
    reject_ambiguous_aliases(declared, object)?;
    if object.keys().all(|key| accepted.contains(key)) {
        return Ok(Cow::Borrowed(params));
    }
    Ok(Cow::Owned(serde_json::Value::Object(
        object
            .iter()
            .filter(|(key, _)| accepted.contains(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )))
}

fn reject_ambiguous_aliases(
    declared: &BTreeSet<String>,
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), ParamContractError> {
    for exact in declared {
        let alias = temper_spec::naming::to_pascal_case(exact);
        if alias != *exact && object.contains_key(exact) && object.contains_key(&alias) {
            return Err(ParamContractError::AmbiguousAlias {
                exact: exact.clone(),
                alias,
            });
        }
    }
    Ok(())
}

/// Return request-body keys that the action does not declare.
///
/// This is the external rejection equivalent of
/// [`restrict_to_declared_params`]. A known action without declaration metadata
/// is an internal error and fails closed; unknown actions remain the evaluator's
/// responsibility.
pub(crate) fn undeclared_param_keys(
    table: &TransitionTable,
    action: &str,
    body: &serde_json::Value,
) -> Result<Vec<String>, ParamContractError> {
    let Some(declared) = table.declared_params(action) else {
        if table.has_action(action) {
            return Err(ParamContractError::MissingMetadata(action.to_string()));
        }
        return Ok(Vec::new());
    };
    let accepted = accepted_param_keys(declared)?;
    let Some(object) = body.as_object() else {
        return Ok(Vec::new());
    };
    reject_ambiguous_aliases(declared, object)?;
    let mut undeclared: Vec<String> = object
        .keys()
        .filter(|key| !accepted.contains(*key))
        .cloned()
        .collect();
    undeclared.sort();
    Ok(undeclared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_actor::effects::process_action;
    use crate::entity_actor::types::EntityState;

    fn work_summary_table() -> TransitionTable {
        TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "WorkSummary"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "AttachVector"
kind = "input"
from = ["Done"]
to = "Done"
params = ["semantic_vector", "semantic_vector_model"]
"#,
        )
    }

    fn state(entity_type: &str) -> EntityState {
        EntityState {
            entity_type: entity_type.into(),
            entity_id: "arn-247".into(),
            status: "Done".into(),
            item_count: 0,
            counters: Default::default(),
            booleans: Default::default(),
            lists: Default::default(),
            fields: serde_json::json!({}),
            events: Default::default(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: Default::default(),
        }
    }

    #[test]
    fn undeclared_body_keys_do_not_mutate_fields() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(247);
        let table = work_summary_table();
        let mut state = state("WorkSummary");
        state.fields = serde_json::json!({
            "goal": "original goal",
            "outcome": "shipped honestly",
        });
        let result = process_action(
            &mut state,
            &table,
            "AttachVector",
            &serde_json::json!({
                "semantic_vector": "[0.1,0.2]",
                "semantic_vector_model": "text-embedding-3-small",
                "goal": "HIJACKED",
                "outcome": "REWRITTEN",
            }),
        );
        assert!(result.success);
        assert_eq!(state.fields["semantic_vector"], "[0.1,0.2]");
        assert_eq!(
            state.fields["semantic_vector_model"],
            "text-embedding-3-small"
        );
        assert_eq!(state.fields["goal"], "original goal");
        assert_eq!(state.fields["outcome"], "shipped honestly");
    }

    #[test]
    fn filter_drops_undeclared_and_keeps_declared() {
        let table = work_summary_table();
        let params = serde_json::json!({
            "semantic_vector": "v",
            "semantic_vector_model": "m",
            "goal": "smuggled",
        });
        let restricted =
            restrict_to_declared_params(&table, "AttachVector", &params).expect("valid metadata");
        let object = restricted.as_object().expect("object");
        assert!(object.contains_key("semantic_vector"));
        assert!(object.contains_key("semantic_vector_model"));
        assert!(!object.contains_key("goal"));
    }

    #[test]
    fn unknown_action_is_left_for_the_evaluator() {
        let table = work_summary_table();
        let params = serde_json::json!({ "anything": "kept" });
        assert_eq!(
            restrict_to_declared_params(&table, "NotADeclaredAction", &params)
                .expect("unknown action is left to the evaluator")
                .as_ref(),
            &params,
        );
        assert!(
            undeclared_param_keys(&table, "NotADeclaredAction", &params)
                .expect("unknown action is not a metadata error")
                .is_empty()
        );
    }

    #[test]
    fn transient_fields_must_also_be_declared() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "Repository"
states = ["Active"]
initial = "Active"

[[action]]
name = "IngestPack"
kind = "input"
from = ["Active"]
to = "Active"
params = ["PackBytes"]
"#,
        );
        let params = serde_json::json!({ "PackBytes": "base64", "Bogus": "drop-me" });
        let restricted =
            restrict_to_declared_params(&table, "IngestPack", &params).expect("valid metadata");
        let object = restricted.as_object().expect("object");
        assert!(object.contains_key("PackBytes"));
        assert!(!object.contains_key("Bogus"));
    }

    #[test]
    fn canonical_pascal_alias_is_accepted() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "Directory"
states = ["Active"]
initial = "Active"

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
to = "Active"
params = ["name", "path", "parent_id", "workspace_id"]
"#,
        );
        let params = serde_json::json!({
            "Name": "/",
            "Path": "/",
            "WorkspaceId": "wsA",
            "ParentId": "d-1",
            "Smuggled": "drop-me",
        });
        let restricted =
            restrict_to_declared_params(&table, "Create", &params).expect("valid metadata");
        let object = restricted.as_object().expect("object");
        assert!(object.contains_key("Name"));
        assert!(object.contains_key("WorkspaceId"));
        assert!(object.contains_key("ParentId"));
        assert!(!object.contains_key("Smuggled"));
        assert_eq!(
            undeclared_param_keys(&table, "Create", &params).expect("metadata"),
            vec!["Smuggled"],
        );
    }

    #[test]
    fn known_action_without_metadata_fails_closed() {
        let mut table = work_summary_table();
        table.action_params.clear();
        let params = serde_json::json!({ "goal": "smuggled" });
        assert!(undeclared_param_keys(&table, "AttachVector", &params).is_err());

        let mut entity_state = state("WorkSummary");
        let result = process_action(&mut entity_state, &table, "AttachVector", &params);
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("missing declared-parameter metadata"))
        );
        assert!(entity_state.fields.get("goal").is_none());
    }

    #[test]
    fn broad_case_and_underscore_aliases_are_rejected_deterministically() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "Account"
states = ["Active"]
initial = "Active"

[[action]]
name = "UpdateOwner"
kind = "input"
from = ["Active"]
to = "Active"
params = ["user_id"]
"#,
        );
        let params = serde_json::json!({
            "user_id": "snake",
            "userid": "collision",
            "USER_ID": "case-folded",
        });
        assert_eq!(
            undeclared_param_keys(&table, "UpdateOwner", &params).expect("metadata"),
            vec!["USER_ID", "userid"],
        );
    }

    #[test]
    fn exact_and_pascal_alias_together_are_rejected_before_mutation() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "Account"
states = ["Done"]
initial = "Done"

[[action]]
name = "UpdateOwner"
kind = "input"
from = ["Done"]
to = "Done"
params = ["user_id"]
"#,
        );
        let params = serde_json::json!({
            "user_id": "attacker",
            "UserId": "victim",
        });
        assert!(matches!(
            undeclared_param_keys(&table, "UpdateOwner", &params),
            Err(ParamContractError::AmbiguousAlias { .. })
        ));

        let mut state = state("Account");
        let result = process_action(&mut state, &table, "UpdateOwner", &params);
        assert!(!result.success);
        assert!(result.error.as_deref().is_some_and(|error| {
            error.contains("both 'user_id'") && error.contains("'UserId'")
        }));
        assert!(state.fields.get("user_id").is_none());
        assert!(state.fields.get("UserId").is_none());
    }

    #[test]
    fn colliding_declared_aliases_fail_closed() {
        let mut table = work_summary_table();
        table.action_params.insert(
            "AttachVector".to_string(),
            BTreeSet::from(["user_id".to_string(), "UserId".to_string()]),
        );
        let params = serde_json::json!({ "UserId": "ambiguous" });
        assert!(matches!(
            undeclared_param_keys(&table, "AttachVector", &params),
            Err(ParamContractError::DeclarationAliasCollision { .. })
        ));

        let mut entity_state = state("WorkSummary");
        let result = process_action(&mut entity_state, &table, "AttachVector", &params);
        assert!(!result.success);
        assert!(result.error.as_deref().is_some_and(|error| {
            error.contains("'user_id'")
                && error.contains("'UserId'")
                && error.contains("spelling 'UserId'")
        }));
        assert!(entity_state.fields.get("UserId").is_none());

        let null_body = serde_json::Value::Null;
        assert!(matches!(
            undeclared_param_keys(&table, "AttachVector", &null_body),
            Err(ParamContractError::DeclarationAliasCollision { .. })
        ));
        let mut null_state = state("WorkSummary");
        let null_result = process_action(&mut null_state, &table, "AttachVector", &null_body);
        assert!(!null_result.success);
        assert!(
            null_state
                .fields
                .as_object()
                .is_some_and(|fields| fields.is_empty())
        );
    }
}
