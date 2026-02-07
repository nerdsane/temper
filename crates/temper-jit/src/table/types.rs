//! Core types for transition tables.
//!
//! A [`TransitionTable`] encodes the complete set of transition rules for a single
//! entity type as DATA, not code. It can be hot-swapped per-actor without restart.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A transition table: state machine transitions as DATA, not code.
/// Can be hot-swapped per-actor without restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionTable {
    /// The entity this table governs (e.g. "Order").
    pub entity_name: String,
    /// All valid state values.
    pub states: Vec<String>,
    /// The state an entity starts in.
    pub initial_state: String,
    /// Ordered list of transition rules.
    pub rules: Vec<TransitionRule>,
}

/// A single transition rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRule {
    /// Action name (e.g. "SubmitOrder").
    pub name: String,
    /// States this transition may fire from.
    pub from_states: Vec<String>,
    /// Target state after the transition (if deterministic).
    pub to_state: Option<String>,
    /// Guard condition evaluated before the transition fires.
    pub guard: Guard,
    /// Effects applied after the transition fires.
    pub effects: Vec<Effect>,
}

/// A guard condition (evaluated before a transition fires).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Guard {
    /// No guard -- always passes.
    Always,
    /// Current state must be in the given set.
    StateIn(Vec<String>),
    /// `items.len() >= N`.
    ItemCountMin(usize),
    /// All inner guards must pass.
    And(Vec<Guard>),
}

/// An effect applied after a transition fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Effect {
    /// Change the entity status.
    SetState(String),
    /// Add an item (increment item count).
    IncrementItems,
    /// Remove an item (decrement item count).
    DecrementItems,
    /// Emit a named event.
    EmitEvent(String),
}

/// The result of evaluating a transition.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionResult {
    /// The new state after the transition (may be unchanged).
    pub new_state: String,
    /// Effects that were applied.
    pub effects: Vec<Effect>,
    /// Whether the transition succeeded.
    pub success: bool,
}

// ---------------------------------------------------------------------------
// Guard evaluation
// ---------------------------------------------------------------------------

impl Guard {
    /// Evaluate this guard against the current runtime context.
    pub fn evaluate(&self, current_state: &str, item_count: usize) -> bool {
        match self {
            Guard::Always => true,
            Guard::StateIn(states) => states.iter().any(|s| s == current_state),
            Guard::ItemCountMin(n) => item_count >= *n,
            Guard::And(guards) => guards.iter().all(|g| g.evaluate(current_state, item_count)),
        }
    }
}
