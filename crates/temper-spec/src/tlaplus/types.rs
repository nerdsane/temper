use serde::{Deserialize, Serialize};

/// Extracted state machine structure from a TLA+ specification.
/// This is NOT a full TLA+ parser — it extracts the structured elements
/// that Temper's codegen needs: states, transitions, guards, invariants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMachine {
    /// Module name from the TLA+ spec.
    pub module_name: String,
    /// All declared state values.
    pub states: Vec<String>,
    /// All declared transitions.
    pub transitions: Vec<Transition>,
    /// Safety invariants (properties that must always hold).
    pub invariants: Vec<Invariant>,
    /// Liveness properties (something good eventually happens).
    pub liveness_properties: Vec<LivenessProperty>,
    /// Constants declared in the spec.
    pub constants: Vec<String>,
    /// Variables declared in the spec.
    pub variables: Vec<String>,
}

/// A state machine transition (action).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    /// The action name (e.g., "SubmitOrder").
    pub name: String,
    /// States this transition can be taken from (extracted from guards).
    pub from_states: Vec<String>,
    /// The target state after this transition (if deterministic).
    pub to_state: Option<String>,
    /// Raw guard expression (the TLA+ precondition).
    pub guard_expr: String,
    /// Whether this transition has parameters.
    pub has_parameters: bool,
    /// Raw effect expression (what changes).
    pub effect_expr: String,
}

/// A safety invariant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invariant {
    /// Invariant name (e.g., "ShipRequiresPayment").
    pub name: String,
    /// Raw TLA+ expression.
    pub expr: String,
}

/// A liveness property.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LivenessProperty {
    /// Property name.
    pub name: String,
    /// Raw TLA+ expression.
    pub expr: String,
}
