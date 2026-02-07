//! I/O Automaton types — the specification data model.
//!
//! Based on Lynch-Tuttle I/O Automata: a labeled state transition system
//! where each action has a precondition (predicate on pre-state) and an
//! effect (state change program).

use serde::{Deserialize, Serialize};

/// A complete I/O Automaton specification for a single entity type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Automaton {
    /// Automaton metadata.
    pub automaton: AutomatonMeta,
    /// State variable declarations.
    #[serde(default)]
    pub state: Vec<StateVar>,
    /// All actions (input, output, internal).
    #[serde(default, rename = "action")]
    pub actions: Vec<Action>,
    /// Safety invariants (must always hold).
    #[serde(default, rename = "invariant")]
    pub invariants: Vec<Invariant>,
}

/// Automaton metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomatonMeta {
    /// Entity name (e.g., "Order").
    pub name: String,
    /// The status state space (all valid values).
    pub states: Vec<String>,
    /// Initial status value.
    pub initial: String,
}

/// A state variable declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateVar {
    /// Variable name.
    pub name: String,
    /// Type: "status", "counter", "set", "string", "bool".
    #[serde(rename = "type")]
    pub var_type: String,
    /// Initial value (as a string, parsed by type).
    pub initial: String,
}

/// An action in the I/O Automaton.
///
/// Actions are classified by `kind`:
/// - `input`: arrives from the environment (HTTP request), always enabled
/// - `output`: emitted to the environment (event to Postgres, span to ClickHouse)
/// - `internal`: private state transition (the state machine step)
///
/// Each action has a precondition (guard) and effects (state changes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    /// Action name (e.g., "SubmitOrder").
    pub name: String,
    /// Action kind: "input", "output", or "internal".
    #[serde(default = "default_internal")]
    pub kind: String,
    /// Precondition: states from which this action can fire.
    #[serde(default)]
    pub from: Vec<String>,
    /// Effect: the target state after this action fires.
    pub to: Option<String>,
    /// Additional guard conditions.
    #[serde(default)]
    pub guard: Vec<Guard>,
    /// Effects beyond state change.
    #[serde(default)]
    pub effect: Vec<Effect>,
    /// Parameters this action accepts.
    #[serde(default)]
    pub params: Vec<String>,
    /// Agent hint for this action.
    pub hint: Option<String>,
}

fn default_internal() -> String {
    "internal".to_string()
}

/// A guard condition (precondition predicate on pre-state).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Guard {
    /// Status must be one of these values.
    #[serde(rename = "state_in")]
    StateIn { values: Vec<String> },
    /// A counter variable must be >= this value.
    #[serde(rename = "min_count")]
    MinCount { var: String, min: usize },
    /// A counter variable must be < this value.
    #[serde(rename = "max_count")]
    MaxCount { var: String, max: usize },
    /// A boolean variable must be true.
    #[serde(rename = "is_true")]
    IsTrue { var: String },
}

/// An effect (state change in the post-state).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Effect {
    /// Increment a counter variable.
    #[serde(rename = "increment")]
    Increment { var: String },
    /// Decrement a counter variable.
    #[serde(rename = "decrement")]
    Decrement { var: String },
    /// Set a boolean variable.
    #[serde(rename = "set_bool")]
    SetBool { var: String, value: bool },
    /// Emit a named event (output action).
    #[serde(rename = "emit")]
    Emit { event: String },
}

/// A safety invariant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invariant {
    /// Invariant name.
    pub name: String,
    /// States in which this invariant is checked (trigger states).
    /// If empty, checked in all states.
    #[serde(default)]
    pub when: Vec<String>,
    /// The assertion (a simple expression).
    pub assert: String,
}
