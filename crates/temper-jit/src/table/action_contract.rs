//! Pure validation shared by native execution and deterministic simulation.

use super::TransitionTable;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use temper_spec::automaton::ActionConstraint;

/// Allowed parameters and atomic pre-state checks for one action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionContract {
    /// Incoming keys explicitly declared in the IOA action.
    pub params: BTreeSet<String>,
    /// Preconditions checked before effects and field synchronization.
    pub constraints: Vec<ActionConstraint>,
}

fn unsigned(value: &Value) -> Option<u128> {
    value.as_u64().map(u128::from)
}

fn matches_field(param: &Value, field: &Value) -> bool {
    if field.is_number() {
        matches!((unsigned(param), unsigned(field)), (Some(a), Some(b)) if a == b)
    } else {
        param == field
    }
}

impl TransitionTable {
    /// Strict entities begin with identity only; data enters through declared actions.
    pub fn validate_initial_fields(&self, fields: &Value) -> Result<(), String> {
        if !self.strict_action_params {
            return Ok(());
        }
        let fields = fields
            .as_object()
            .ok_or_else(|| "Entity creation requires a JSON object".to_owned())?;
        for (key, value) in fields {
            match key.as_str() {
                "id" | "Id" => {}
                "status" | "Status" if value.as_str() == Some(self.initial_state.as_str()) => {}
                _ => {
                    return Err(format!(
                        "Strict entity creation does not accept field '{key}'; use a declared action"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Check request parameters against the unmodified actor state.
    /// Values are never included in errors because parameters can contain secrets.
    pub fn validate_action_params(
        &self,
        action: &str,
        params: &Value,
        fields: &Value,
        counters: &BTreeMap<String, usize>,
        booleans: &BTreeMap<String, bool>,
    ) -> Result<(), String> {
        let contract = self.action_contracts.get(action);
        if !self.strict_action_params
            && contract.is_none_or(|contract| contract.constraints.is_empty())
        {
            return Ok(());
        }
        let object = params
            .as_object()
            .ok_or_else(|| "Action parameters must be a JSON object".to_owned())?;
        let contract = contract
            .ok_or_else(|| format!("Action '{action}' has no declared parameter contract"))?;
        if self.strict_action_params {
            for key in object.keys() {
                if !contract.params.contains(key) {
                    return Err(format!(
                        "Action '{action}' does not accept parameter '{key}'"
                    ));
                }
            }
        }
        for constraint in &contract.constraints {
            let name = constraint.param();
            let param = object
                .get(name)
                .ok_or_else(|| format!("Action '{action}' requires parameter '{name}'"))?;
            let field = constraint.field().and_then(|name| {
                counters
                    .get(name)
                    .map(|value| Value::from(*value as u64))
                    .or_else(|| booleans.get(name).map(|value| Value::from(*value)))
                    .or_else(|| fields.get(name).cloned())
            });
            let passed = match constraint {
                ActionConstraint::ParamNonempty { .. } => {
                    param.as_str().is_some_and(|value| !value.trim().is_empty())
                }
                ActionConstraint::ParamEqualsField { .. } => field
                    .as_ref()
                    .is_some_and(|field| matches_field(param, field)),
                ActionConstraint::ParamNotEqualsField { .. } => {
                    field.as_ref().is_some_and(|field| {
                        !field.is_null() && !param.is_null() && !matches_field(param, field)
                    })
                }
                ActionConstraint::ParamGreaterThanField { .. } => {
                    matches!((unsigned(param), field.as_ref().and_then(unsigned)), (Some(a), Some(b)) if a > b)
                }
            };
            if !passed {
                return Err(format!(
                    "Action '{action}' constraint failed for parameter '{name}'"
                ));
            }
        }
        Ok(())
    }
}
