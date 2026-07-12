//! Declared action-parameter boundary shared by every dispatch path.
//!
//! ARN-247: the runtime must only project an action's *declared* params into
//! entity fields, so a verified invariant is an enforced one. This lives in
//! `temper-jit` (next to [`TransitionTable`]) so both the legacy in-process
//! runtime (`temper-server`) and the pg-actor runtime (`temper-actor-runtime`)
//! filter through the *same* logic, rather than each re-deriving it.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::TransitionTable;

/// A request-body contract violation at the action boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamContractError {
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

impl std::error::Error for ParamContractError {}

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

/// Restrict caller-supplied action params to the action's declared set (drop).
///
/// Kernel-injected params survive by construction: derived values are added
/// after this filter, while spawn `copy_fields` must be declared by the target
/// initial action (the bundle lint enforces that contract). A known action
/// missing declaration metadata fails closed (`MissingMetadata`); unknown
/// actions pass through and remain the evaluator's responsibility.
pub fn restrict_to_declared_params<'a>(
    table: &TransitionTable,
    action: &str,
    params: &'a serde_json::Value,
) -> Result<Cow<'a, serde_json::Value>, ParamContractError> {
    let Some(declared) = table.declared_params(action) else {
        if table.has_action(action) {
            return Err(ParamContractError::MissingMetadata(action.to_string()));
        }
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

/// Return request-body keys that the action does not declare (reject).
///
/// This is the external-rejection equivalent of [`restrict_to_declared_params`].
/// A known action without declaration metadata is an internal error and fails
/// closed; unknown actions remain the evaluator's responsibility.
pub fn undeclared_param_keys(
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

    fn attach_vector_table() -> TransitionTable {
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

    #[test]
    fn drops_undeclared_keeps_declared() {
        let table = attach_vector_table();
        let params = serde_json::json!({
            "semantic_vector": "v",
            "semantic_vector_model": "m",
            "goal": "smuggled",
        });
        let restricted = restrict_to_declared_params(&table, "AttachVector", &params).unwrap();
        let object = restricted.as_object().unwrap();
        assert!(object.contains_key("semantic_vector"));
        assert!(object.contains_key("semantic_vector_model"));
        assert!(!object.contains_key("goal"));
    }

    #[test]
    fn pascal_alias_of_snake_param_is_accepted() {
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
params = ["name", "parent_id", "workspace_id"]
"#,
        );
        let params =
            serde_json::json!({ "Name": "/", "WorkspaceId": "w", "ParentId": "d", "X": 1 });
        let object = restrict_to_declared_params(&table, "Create", &params)
            .unwrap()
            .into_owned();
        let object = object.as_object().unwrap().clone();
        assert!(object.contains_key("WorkspaceId"));
        assert!(object.contains_key("ParentId"));
        assert!(!object.contains_key("X"));
        assert_eq!(
            undeclared_param_keys(&table, "Create", &params).unwrap(),
            vec!["X"]
        );
    }

    #[test]
    fn known_action_without_metadata_fails_closed() {
        let mut table = attach_vector_table();
        table.action_params.clear();
        let params = serde_json::json!({ "goal": "smuggled" });
        assert!(matches!(
            restrict_to_declared_params(&table, "AttachVector", &params),
            Err(ParamContractError::MissingMetadata(_))
        ));
        assert!(matches!(
            undeclared_param_keys(&table, "AttachVector", &params),
            Err(ParamContractError::MissingMetadata(_))
        ));
    }

    #[test]
    fn unknown_action_passes_through() {
        let table = attach_vector_table();
        let params = serde_json::json!({ "anything": "kept" });
        assert_eq!(
            restrict_to_declared_params(&table, "Ghost", &params)
                .unwrap()
                .as_ref(),
            &params
        );
        assert!(
            undeclared_param_keys(&table, "Ghost", &params)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ambiguous_alias_is_rejected() {
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
        let params = serde_json::json!({ "user_id": "a", "UserId": "b" });
        assert!(matches!(
            restrict_to_declared_params(&table, "UpdateOwner", &params),
            Err(ParamContractError::AmbiguousAlias { .. })
        ));
        assert!(matches!(
            undeclared_param_keys(&table, "UpdateOwner", &params),
            Err(ParamContractError::AmbiguousAlias { .. })
        ));
    }
}
