//! Pure validation shared by native execution and deterministic simulation.

use super::TransitionTable;
use super::refusal::{ContractError, Refusal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use temper_spec::automaton::ActionConstraint;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_initial_identity_is_a_nonempty_string() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "Identity"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
"#,
        );
        for invalid in [
            json!({"x":1}),
            json!([]),
            json!(1),
            json!(false),
            Value::Null,
            json!(""),
        ] {
            for key in ["Id", "id"] {
                assert!(
                    table
                        .validate_initial_fields(&json!({key:invalid}))
                        .is_err()
                );
            }
        }
        assert!(
            table
                .validate_initial_fields(&json!({"Id":"valid","id":"valid"}))
                .is_ok()
        );
    }

    #[test]
    fn a_constraint_reads_a_missing_declared_value_as_its_initial_value() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "PersistedSequence"
states = ["Active"]
initial = "Active"
strict_action_params = true
[[state]]
name = "sequence"
type = "counter"
initial = 7
[[action]]
name = "Advance"
kind = "input"
from = ["Active"]
params = ["expected"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected"
field = "sequence"
"#,
        );
        let params = json!({"expected":7});
        // A record made before `sequence` was declared stores no value for it.
        let fields = json!({});
        let mut counters = BTreeMap::new();
        let booleans = BTreeMap::new();
        assert!(
            table
                .validate_action_params("Advance", &params, &fields, &counters, &booleans)
                .is_ok()
        );
        assert!(
            table
                .validate_action_params(
                    "Advance",
                    &json!({"expected":0}),
                    &fields,
                    &counters,
                    &booleans
                )
                .is_err()
        );
        // A stored value wins over the declaration.
        counters.insert("sequence".into(), 7);
        assert!(
            table
                .validate_action_params("Advance", &params, &fields, &counters, &booleans)
                .is_ok()
        );
        counters.insert("sequence".into(), 8);
        assert!(
            table
                .validate_action_params("Advance", &params, &fields, &counters, &booleans)
                .is_err()
        );
    }

    #[test]
    fn greater_than_requires_a_nonnegative_parameter_even_for_signed_state() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "SignedSequence"
states = ["Active"]
initial = "Active"
strict_action_params = true
[[state]]
name = "sequence"
type = "int"
initial = -3
[[action]]
name = "Advance"
kind = "input"
from = ["Active"]
params = ["expected"]
[[action.constraints]]
kind = "param_greater_than_field"
param = "expected"
field = "sequence"
"#,
        );
        let fields = json!({"sequence":-3});
        let counters = BTreeMap::new();
        let booleans = BTreeMap::new();
        assert!(
            table
                .validate_action_params(
                    "Advance",
                    &json!({"expected":-2}),
                    &fields,
                    &counters,
                    &booleans
                )
                .is_err()
        );
        assert!(
            table
                .validate_action_params(
                    "Advance",
                    &json!({"expected":0}),
                    &fields,
                    &counters,
                    &booleans
                )
                .is_ok()
        );
    }
}

/// Allowed parameters and atomic pre-state checks for one action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionContract {
    /// Incoming keys explicitly declared in the IOA action.
    pub params: BTreeSet<String>,
    /// Explicit types only; bare parameter names retain constraint-inferred semantics.
    #[serde(default)]
    pub param_types: BTreeMap<String, temper_spec::automaton::VarType>,
    /// Values resolved by authenticated dispatch, never accepted from the caller.
    #[serde(default)]
    pub param_sources: BTreeMap<String, temper_spec::automaton::ParameterSource>,
    /// States in which this action brings an armed state timer forward.
    /// Such a move needs a recorded event even when it changes no fields.
    #[serde(default)]
    pub fire_now_in: BTreeSet<String>,
    /// Preconditions checked before effects and field synchronization.
    pub constraints: Vec<ActionConstraint>,
}

/// Typed initial values, retained when a transition table is persisted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InitialValues {
    /// Natural-number counters.
    pub counters: BTreeMap<String, usize>,
    /// Boolean state variables.
    pub booleans: BTreeMap<String, bool>,
    /// List and set state variables.
    pub lists: BTreeMap<String, Vec<String>>,
    /// Ordinary field values.
    pub fields: BTreeMap<String, Value>,
}

impl InitialValues {
    pub(crate) fn from_declarations(state: &[temper_spec::automaton::StateVar]) -> Self {
        let mut values = Self::default();
        for var in state {
            use temper_spec::automaton::Initial;
            match &var.initial {
                Initial::Counter(n) => {
                    values.counters.insert(var.name.clone(), *n);
                }
                Initial::Bool(b) => {
                    values.booleans.insert(var.name.clone(), *b);
                }
                Initial::List(items) => {
                    values.lists.insert(var.name.clone(), items.clone());
                }
                Initial::String(_) | Initial::Int(_) => {
                    values
                        .fields
                        .insert(var.name.clone(), var.initial.to_json());
                }
            }
        }
        values
    }

    // A record made before a spec version declared a variable stores no
    // value for it. Every reader of state goes through these, so a guard, a
    // constraint and an effect read the declared initial value, as they
    // would had the variable been declared when the record was made.

    /// A counter's stored value, else its declared initial value.
    pub fn counter(&self, stored: &BTreeMap<String, usize>, name: &str) -> Option<usize> {
        stored
            .get(name)
            .or_else(|| self.counters.get(name))
            .copied()
    }

    /// A boolean's stored value, else its declared initial value.
    pub fn boolean(&self, stored: &BTreeMap<String, bool>, name: &str) -> Option<bool> {
        stored
            .get(name)
            .or_else(|| self.booleans.get(name))
            .copied()
    }

    /// A list's stored items, else its declared initial items.
    pub fn list<'a>(
        &'a self,
        stored: &'a BTreeMap<String, Vec<String>>,
        name: &str,
    ) -> Option<&'a [String]> {
        stored
            .get(name)
            .or_else(|| self.lists.get(name))
            .map(Vec::as_slice)
    }

    /// A list to change in place, starting from its declared initial items
    /// when the record stores none.
    pub fn list_mut<'a>(
        &self,
        stored: &'a mut BTreeMap<String, Vec<String>>,
        name: &str,
    ) -> &'a mut Vec<String> {
        stored
            .entry(name.to_string())
            .or_insert_with(|| self.lists.get(name).cloned().unwrap_or_default())
    }

    /// A field's stored value, if any, else its declared initial value.
    pub fn field<'a>(&'a self, stored: Option<&'a Value>, name: &str) -> Option<&'a Value> {
        stored.or_else(|| self.fields.get(name))
    }
}

fn integer(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}

fn matches_field(param: &Value, field: &Value, counter: bool) -> Option<bool> {
    match (param, field) {
        (Value::Number(_), Value::Number(_)) => {
            if counter && param.as_u64().is_none() {
                return None;
            }
            Some(integer(param)? == integer(field)?)
        }
        (Value::String(_), Value::String(_))
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Array(_), Value::Array(_))
        | (Value::Object(_), Value::Object(_)) => Some(param == field),
        _ => None,
    }
}

impl TransitionTable {
    /// Whether any action requires the persisted input-contract semantics.
    pub fn has_input_contracts(&self) -> bool {
        self.strict_action_params
            || self
                .action_contracts
                .values()
                .any(|contract| !contract.constraints.is_empty())
    }

    /// Materialize declarations once when creating a fresh contracted actor.
    /// Recovery must retain persisted state, including missing fields.
    pub fn initialize_declared_fields(
        &self,
        fields: &mut Value,
        counters: &mut BTreeMap<String, usize>,
        booleans: &mut BTreeMap<String, bool>,
    ) {
        if !self.strict_action_params
            && self
                .action_contracts
                .values()
                .all(|contract| contract.constraints.is_empty())
        {
            return;
        }
        counters.extend(self.initial_values.counters.clone());
        booleans.extend(self.initial_values.booleans.clone());
        if fields.is_null() {
            *fields = Value::Object(Default::default());
        }
        if let Some(fields) = fields.as_object_mut() {
            for (name, value) in &self.initial_values.fields {
                fields.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
    }

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
                "id" | "Id" => {
                    if value.as_str().is_none_or(str::is_empty) {
                        return Err("Strict entity identity must be a nonempty string".into());
                    }
                }
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
    ) -> Result<(), ContractError> {
        let contract = self.action_contracts.get(action);
        if !self.strict_action_params
            && contract.is_none_or(|contract| contract.constraints.is_empty())
        {
            return Ok(());
        }
        let object = params.as_object().ok_or_else(|| {
            ContractError::message("Action parameters must be a JSON object".into())
        })?;
        let contract = contract.ok_or_else(|| {
            ContractError::message(format!(
                "Action '{action}' has no declared parameter contract"
            ))
        })?;
        if self.strict_action_params {
            for key in object.keys() {
                if !contract.params.contains(key) {
                    return Err(ContractError::message(format!(
                        "Action '{action}' does not accept parameter '{key}'"
                    )));
                }
            }
        }
        for (name, kind) in &contract.param_types {
            if let Some(value) = object.get(name) {
                use temper_spec::automaton::VarType;
                let valid = match kind {
                    VarType::String => value.is_string(),
                    VarType::Bool => value.is_boolean(),
                    VarType::Int => value.as_i64().is_some(),
                    VarType::Counter => value.as_u64().is_some(),
                    VarType::List => value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string)),
                };
                if !valid {
                    return Err(ContractError::message(format!(
                        "Action '{action}' parameter '{name}' has an invalid type"
                    )));
                }
            }
        }
        for constraint in &contract.constraints {
            let name = constraint.param();
            let param = object.get(name).ok_or_else(|| ContractError {
                message: format!("Action '{action}' requires parameter '{name}'"),
                refusal: Some(Refusal::MissingParameter {
                    parameter: name.to_string(),
                }),
            })?;
            let declared = &self.initial_values;
            let field = constraint.field().and_then(|name| {
                declared
                    .counter(counters, name)
                    .map(|value| Value::from(value as u64))
                    .or_else(|| declared.boolean(booleans, name).map(Value::from))
                    .or_else(|| declared.field(fields.get(name), name).cloned())
            });
            let counter = constraint.field().is_some_and(|name| {
                self.initial_values.counters.contains_key(name) || counters.contains_key(name)
            });
            let passed = match constraint {
                ActionConstraint::ParamNonempty { .. } => {
                    param.as_str().is_some_and(|value| !value.trim().is_empty())
                }
                ActionConstraint::ParamEqualsField { .. } => field
                    .as_ref()
                    .is_some_and(|field| matches_field(param, field, counter) == Some(true)),
                ActionConstraint::ParamNotEqualsField { .. } => field
                    .as_ref()
                    .is_some_and(|field| matches_field(param, field, counter) == Some(false)),
                ActionConstraint::ParamGreaterThanField { .. } => {
                    param.as_u64().is_some()
                        && matches!((integer(param), field.as_ref().and_then(integer)), (Some(a), Some(b)) if a > b)
                }
            };
            if !passed {
                return Err(ContractError {
                    message: format!("Action '{action}' constraint failed for parameter '{name}'"),
                    refusal: Some(Refusal::ParameterConstraint {
                        parameter: name.to_string(),
                    }),
                });
            }
        }
        Ok(())
    }
}
