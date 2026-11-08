//! An action's declared parameters (nerdsane/temper#351, ARN-247).
//!
//! The verifier models an action as receiving only its declared `params`, but
//! the runtime writes every request-body key into fields. These functions say
//! which keys an action does not declare, and keep only the declared ones.
//! A declared name is accepted as written and as its PascalCase CSDL alias
//! (`error_message` and `ErrorMessage`); broader case folding is not, since it
//! would accept spellings the verifier never saw.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::TransitionTable;

/// A request body that the declared parameters cannot be read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamContractError {
    /// Both spellings of one parameter were sent, with different values.
    AmbiguousAlias { exact: String, alias: String },
    /// Two declared parameters accept the same spelling.
    DeclarationAliasCollision {
        first: String,
        second: String,
        accepted: String,
    },
}

impl std::fmt::Display for ParamContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AmbiguousAlias { exact, alias } => write!(
                formatter,
                "action body supplies both '{exact}' and its canonical alias '{alias}' with different values"
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

/// The body keys a declared parameter set accepts: each name and its PascalCase alias.
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

/// Both spellings of a parameter are ambiguous only when their values differ:
/// a caller sending both with one value (for query compatibility) is benign.
fn reject_ambiguous_aliases(
    declared: &BTreeSet<String>,
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), ParamContractError> {
    for exact in declared {
        let alias = temper_spec::naming::to_pascal_case(exact);
        if alias == *exact {
            continue;
        }
        if let (Some(exact_value), Some(alias_value)) = (object.get(exact), object.get(&alias))
            && exact_value != alias_value
        {
            return Err(ParamContractError::AmbiguousAlias {
                exact: exact.clone(),
                alias,
            });
        }
    }
    Ok(())
}

/// The body keys `action` does not declare, sorted. An action the table does
/// not know, or a body that is not an object, has none.
pub fn undeclared_param_keys(
    table: &TransitionTable,
    action: &str,
    body: &serde_json::Value,
) -> Result<Vec<String>, ParamContractError> {
    let Some(contract) = table.action_contracts.get(action) else {
        return Ok(Vec::new());
    };
    let Some(object) = body.as_object() else {
        return Ok(Vec::new());
    };
    let accepted = accepted_param_keys(&contract.params)?;
    reject_ambiguous_aliases(&contract.params, object)?;
    let mut undeclared: Vec<String> = object
        .keys()
        .filter(|key| !accepted.contains(*key))
        .cloned()
        .collect();
    undeclared.sort();
    Ok(undeclared)
}

/// `params` with only the keys `action` declares. Unknown actions and bodies
/// that are not objects pass through unchanged.
pub fn restrict_to_declared_params<'a>(
    table: &TransitionTable,
    action: &str,
    params: &'a serde_json::Value,
) -> Result<Cow<'a, serde_json::Value>, ParamContractError> {
    let Some(contract) = table.action_contracts.get(action) else {
        return Ok(Cow::Borrowed(params));
    };
    let Some(object) = params.as_object() else {
        return Ok(Cow::Borrowed(params));
    };
    let accepted = accepted_param_keys(&contract.params)?;
    reject_ambiguous_aliases(&contract.params, object)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn table() -> TransitionTable {
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
params = ["semantic_vector", "error_message"]
"#,
        )
    }

    #[test]
    fn names_the_keys_an_action_does_not_declare() {
        let body = json!({ "semantic_vector": "v", "goal": "smuggled", "outcome": "x" });
        assert_eq!(
            undeclared_param_keys(&table(), "AttachVector", &body),
            Ok(vec!["goal".to_string(), "outcome".to_string()])
        );
    }

    #[test]
    fn accepts_the_pascal_case_alias() {
        let body = json!({ "SemanticVector": "v", "ErrorMessage": "e" });
        assert_eq!(
            undeclared_param_keys(&table(), "AttachVector", &body),
            Ok(vec![])
        );
    }

    #[test]
    fn both_spellings_are_ambiguous_only_with_different_values() {
        let same = json!({ "error_message": "e", "ErrorMessage": "e" });
        assert_eq!(
            undeclared_param_keys(&table(), "AttachVector", &same),
            Ok(vec![])
        );
        let different = json!({ "error_message": "e", "ErrorMessage": "other" });
        assert!(matches!(
            undeclared_param_keys(&table(), "AttachVector", &different),
            Err(ParamContractError::AmbiguousAlias { .. })
        ));
    }

    #[test]
    fn keeps_only_declared_keys() {
        let body = json!({ "semantic_vector": "v", "goal": "smuggled" });
        let kept = restrict_to_declared_params(&table(), "AttachVector", &body).unwrap();
        assert_eq!(kept.as_ref(), &json!({ "semantic_vector": "v" }));
    }

    #[test]
    fn unknown_actions_and_non_objects_pass_through() {
        let body = json!({ "anything": 1 });
        assert_eq!(
            undeclared_param_keys(&table(), "NotAnAction", &body),
            Ok(vec![])
        );
        assert!(matches!(
            restrict_to_declared_params(&table(), "AttachVector", &json!("text")),
            Ok(Cow::Borrowed(_))
        ));
    }
}
