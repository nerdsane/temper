//! Typed TypeSafe judgments and deterministic semantic preconditions.
//!
//! Inference is external input. This module validates declarations and answers,
//! resolves explicit state bindings, and compares recorded values without I/O.

mod assertion;
mod decimal;
mod response;
mod state;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// One TypeSafe request embedded in a conjunctive IOA guard list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SystemOneGuard {
    /// Requested TypeSafe model or alias.
    pub model: String,
    /// JSON context template containing literals and explicit `ref` bindings.
    pub state: Value,
    /// API-shaped questions, keyed by the names referenced in the assertion.
    pub questions: BTreeMap<String, SystemOneQuestion>,
    /// Typed scalar comparisons over `answers`, combined using `&&`.
    #[serde(rename = "assert")]
    pub assertion: String,
}

/// A TypeSafe question; its native type determines the answer schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SystemOneQuestion {
    /// Select an option from a named rubric.
    Choice {
        /// Text or structured instructions passed through to TypeSafe.
        instructions: Value,
        /// Named options and optional explanatory descriptions.
        criteria: BTreeMap<String, Option<String>>,
    },
    /// Produce a probability-weighted score along an ordered rubric.
    Score {
        /// Text or structured instructions passed through to TypeSafe.
        instructions: Value,
        /// Ordered level descriptions; the first level has index zero.
        criteria: Vec<Value>,
    },
    /// Produce the probability that a proposition is true.
    Noul {
        /// Text or structured instructions passed through to TypeSafe.
        instructions: Value,
        /// Optional descriptions of the true and false outcomes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

/// Optional native rubric for a yes/no question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NoulCriteria {
    /// Description of the affirmative interpretation.
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub affirmative: Option<String>,
    /// Description of the negative interpretation.
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub negative: Option<String>,
}

impl SystemOneGuard {
    /// Validate question schemas, binding syntax, and assertion references.
    pub fn validate(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("system_one model must not be empty".into());
        }
        if self.questions.is_empty() {
            return Err("system_one requires at least one question".into());
        }
        for (name, question) in &self.questions {
            if !identifier(name) {
                return Err(format!("invalid system_one question name '{name}'"));
            }
            question
                .validate()
                .map_err(|e| format!("question '{name}': {e}"))?;
        }
        state::validate(&self.state)?;
        assertion::parse(self)?;
        Ok(())
    }

    /// Stable SHA-256 identity for the full declaration and trusted guard input.
    pub fn key(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("SystemOneGuard is JSON serializable");
        format!("__system_one:{:x}", Sha256::digest(bytes))
    }

    /// Resolve declared state bindings against a pre-transition entity snapshot.
    pub fn resolve_state(&self, entity: &Value, params: &Value) -> Result<Value, String> {
        state::resolve(&self.state, entity, params)
    }

    /// Declared state inputs as `(source, field)` pairs after schema validation.
    /// Runtime adapters use this to load only selected entity storage values;
    /// action parameters and literals remain caller-provided context data.
    pub fn state_references(&self) -> Result<Vec<(String, String)>, String> {
        self.validate()?;
        Ok(state::references(&self.state))
    }

    /// Validate reference names against the deployed entity and action contract.
    pub fn validate_bindings(
        &self,
        entity_fields: &BTreeSet<String>,
        action_params: &BTreeSet<String>,
    ) -> Result<(), String> {
        self.validate()?;
        for (source, name) in state::references(&self.state) {
            let declared = if source == "entity" {
                entity_fields
            } else {
                action_params
            };
            if !declared.contains(&name) {
                return Err(format!(
                    "undeclared system_one state reference '{source}.{name}'"
                ));
            }
        }
        Ok(())
    }

    /// Construct the native API request, omitting Temper bindings and assertions.
    pub fn request(&self, resolved_state: Value) -> Value {
        json!({"model": self.model, "state": resolved_state, "questions": self.questions})
    }

    /// Validate every returned answer before testing the deterministic assertion.
    pub fn evaluate_response(&self, response: &Value) -> Result<bool, String> {
        self.validate()?;
        response::validate(self, response)?;
        assertion::evaluate(self, response)
    }

    /// Whether some well-typed external answer can satisfy the assertion.
    ///
    /// Bounds and comparisons of the same field are consistent. Relationships
    /// between distinct fields are overapproximated for conservative safety proof.
    pub fn assertion_may_hold(&self) -> Result<bool, String> {
        self.validate()?;
        assertion::may_hold(self)
    }
}

impl SystemOneQuestion {
    /// Native API type name used to validate the corresponding answer.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
            Self::Noul { .. } => "noul",
        }
    }

    fn validate(&self) -> Result<(), String> {
        let instructions = match self {
            Self::Choice {
                instructions,
                criteria,
            } => {
                if criteria.is_empty() || criteria.keys().any(|k| k.is_empty()) {
                    return Err("choice requires nonempty option names".into());
                }
                instructions
            }
            Self::Score {
                instructions,
                criteria,
            } => {
                if criteria.len() < 2 || criteria.iter().any(|v| !context_type(v)) {
                    return Err(
                        "score requires at least two string/object/array rubric levels".into(),
                    );
                }
                instructions
            }
            Self::Noul { instructions, .. } => instructions,
        };
        if !context_type(instructions) || instructions.as_str().is_some_and(|s| s.trim().is_empty())
        {
            return Err("instructions must be nonempty text, an object, or an array".into());
        }
        Ok(())
    }
}

fn context_type(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_))
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(super) fn validate_action_declarations(automaton: &super::Automaton) -> Result<(), String> {
    // Legacy duplicate action declarations select the first matching from-state.
    // One semantic action must have one request/assertion list in this version.
    let semantic_names: BTreeSet<_> = automaton
        .actions
        .iter()
        .filter(|action| {
            action
                .guard
                .iter()
                .any(|guard| matches!(guard, super::Guard::SystemOne(_)))
        })
        .map(|action| action.name.as_str())
        .collect();
    for name in semantic_names {
        if automaton
            .actions
            .iter()
            .filter(|action| action.name == name)
            .count()
            != 1
        {
            return Err(format!(
                "system_one action '{name}' requires a unique declaration"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
