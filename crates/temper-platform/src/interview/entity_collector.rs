//! Entity model collector for the developer interview.
//!
//! Accumulates entity definitions (states, actions, guards, invariants) from
//! the interview conversation. The resulting [`EntityModel`] feeds the spec
//! generators to produce IOA TOML, CSDL XML, and Cedar policies.

use serde::{Deserialize, Serialize};

/// Collected entity model from the developer interview.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntityModel {
    /// Entity name in PascalCase (e.g., "Order", "Task").
    pub name: String,
    /// Human-readable description of the entity.
    pub description: String,
    /// Status states the entity can be in.
    pub states: Vec<StateDefinition>,
    /// Actions that transition or modify the entity.
    pub actions: Vec<ActionDefinition>,
    /// Safety invariants that must always hold.
    pub invariants: Vec<InvariantDefinition>,
    /// Additional state variables (counters, booleans, etc.).
    pub state_variables: Vec<StateVariable>,
}

/// A named status state in the entity lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDefinition {
    /// State name in PascalCase (e.g., "Draft", "Submitted").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Whether this is a terminal/final state.
    pub is_terminal: bool,
}

/// An action that can be performed on the entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDefinition {
    /// Action name in PascalCase (e.g., "SubmitOrder").
    pub name: String,
    /// States from which this action can fire.
    pub from_states: Vec<String>,
    /// Target state after this action fires (None for non-transitioning actions).
    pub to_state: Option<String>,
    /// Guard expression (e.g., "items > 0").
    pub guard: Option<String>,
    /// Parameter names this action accepts.
    pub params: Vec<String>,
    /// Human-readable hint for this action.
    pub hint: Option<String>,
    /// Action classification.
    pub kind: ActionKind,
}

/// I/O Automaton action classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ActionKind {
    /// Arrives from the environment (HTTP request).
    Input,
    /// Emitted to the environment (event, notification).
    Output,
    /// Private state transition.
    Internal,
}

impl ActionKind {
    /// Convert to the IOA TOML kind string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::Internal => "internal",
        }
    }
}

/// An additional state variable (counter, boolean, string).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateVariable {
    /// Variable name (e.g., "items", "has_address").
    pub name: String,
    /// Variable type: "counter", "bool", "string", "set".
    pub var_type: String,
    /// Initial value as a string.
    pub initial: String,
}

/// A safety invariant that must hold in specified states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantDefinition {
    /// Invariant name in PascalCase.
    pub name: String,
    /// States in which this invariant is checked.
    pub when: Vec<String>,
    /// The assertion expression (e.g., "items > 0").
    pub assertion: String,
}

impl EntityModel {
    /// Validate the entity model for consistency.
    ///
    /// Returns `Ok(())` if valid, or `Err` with a list of validation errors.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        // Name must be non-empty
        if self.name.is_empty() {
            errors.push("Entity name must not be empty".to_string());
        } else if !self.name.chars().next().is_some_and(|c| c.is_uppercase()) {
            errors.push(format!(
                "Entity name '{}' must start with an uppercase letter (PascalCase)",
                self.name
            ));
        }

        // At least one state
        if self.states.is_empty() {
            errors.push("Entity must have at least one state".to_string());
        }

        // Collect valid state names for cross-referencing
        let valid_states: Vec<&str> = self.states.iter().map(|s| s.name.as_str()).collect();

        // Check action references
        for action in &self.actions {
            for from in &action.from_states {
                if !valid_states.contains(&from.as_str()) {
                    errors.push(format!(
                        "Action '{}' references unknown from-state '{}'",
                        action.name, from
                    ));
                }
            }
            if let Some(ref to) = action.to_state {
                if !valid_states.contains(&to.as_str()) {
                    errors.push(format!(
                        "Action '{}' references unknown to-state '{}'",
                        action.name, to
                    ));
                }
            }
        }

        // Check invariant state references
        for inv in &self.invariants {
            for when_state in &inv.when {
                if !valid_states.contains(&when_state.as_str()) {
                    errors.push(format!(
                        "Invariant '{}' references unknown state '{}'",
                        inv.name, when_state
                    ));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Get the initial state (first non-terminal state, or first state).
    pub fn initial_state(&self) -> Option<&str> {
        self.states
            .iter()
            .find(|s| !s.is_terminal)
            .or(self.states.first())
            .map(|s| s.name.as_str())
    }

    /// Get all state names as strings.
    pub fn state_names(&self) -> Vec<&str> {
        self.states.iter().map(|s| s.name.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_valid_entity() -> EntityModel {
        EntityModel {
            name: "Order".to_string(),
            description: "An e-commerce order".to_string(),
            states: vec![
                StateDefinition {
                    name: "Draft".to_string(),
                    description: "Initial state".to_string(),
                    is_terminal: false,
                },
                StateDefinition {
                    name: "Submitted".to_string(),
                    description: "Order submitted".to_string(),
                    is_terminal: false,
                },
                StateDefinition {
                    name: "Cancelled".to_string(),
                    description: "Order cancelled".to_string(),
                    is_terminal: true,
                },
            ],
            actions: vec![ActionDefinition {
                name: "SubmitOrder".to_string(),
                from_states: vec!["Draft".to_string()],
                to_state: Some("Submitted".to_string()),
                guard: Some("items > 0".to_string()),
                params: vec!["PaymentMethod".to_string()],
                hint: Some("Submit the order".to_string()),
                kind: ActionKind::Internal,
            }],
            invariants: vec![InvariantDefinition {
                name: "SubmitRequiresItems".to_string(),
                when: vec!["Submitted".to_string()],
                assertion: "items > 0".to_string(),
            }],
            state_variables: vec![StateVariable {
                name: "items".to_string(),
                var_type: "counter".to_string(),
                initial: "0".to_string(),
            }],
        }
    }

    #[test]
    fn test_entity_model_default() {
        let model = EntityModel::default();
        assert!(model.name.is_empty());
        assert!(model.states.is_empty());
        assert!(model.actions.is_empty());
        assert!(model.invariants.is_empty());
        assert!(model.state_variables.is_empty());
    }

    #[test]
    fn test_entity_model_validate_valid() {
        let model = make_valid_entity();
        assert!(model.validate().is_ok());
    }

    #[test]
    fn test_entity_model_validate_empty_name() {
        let mut model = make_valid_entity();
        model.name = String::new();
        let errs = model.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("name must not be empty")));
    }

    #[test]
    fn test_entity_model_validate_no_states() {
        let mut model = make_valid_entity();
        model.states.clear();
        let errs = model.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("at least one state")));
    }

    #[test]
    fn test_entity_model_validate_invalid_from_state() {
        let mut model = make_valid_entity();
        model.actions[0].from_states = vec!["Nonexistent".to_string()];
        let errs = model.validate().unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.contains("unknown from-state 'Nonexistent'")));
    }

    #[test]
    fn test_entity_model_validate_invalid_to_state() {
        let mut model = make_valid_entity();
        model.actions[0].to_state = Some("Ghost".to_string());
        let errs = model.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("unknown to-state 'Ghost'")));
    }

    #[test]
    fn test_entity_model_validate_lowercase_name() {
        let mut model = make_valid_entity();
        model.name = "order".to_string();
        let errs = model.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("PascalCase")));
    }

    #[test]
    fn test_initial_state() {
        let model = make_valid_entity();
        assert_eq!(model.initial_state(), Some("Draft"));
    }

    #[test]
    fn test_state_names() {
        let model = make_valid_entity();
        let names = model.state_names();
        assert_eq!(names, vec!["Draft", "Submitted", "Cancelled"]);
    }

    #[test]
    fn test_action_kind_as_str() {
        assert_eq!(ActionKind::Input.as_str(), "input");
        assert_eq!(ActionKind::Output.as_str(), "output");
        assert_eq!(ActionKind::Internal.as_str(), "internal");
    }
}
