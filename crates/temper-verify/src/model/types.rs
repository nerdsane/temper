//! Core types for the Temper verification model.
//!
//! Contains the state, action, transition, invariant, and model struct definitions
//! used by the Stateright model checker.

use std::fmt;

/// The state tracked by the Temper model during verification.
///
/// Consists of the current entity status (e.g. "Draft", "Submitted") and a
/// simple item counter that tracks how many items have been added.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TemperModelState {
    /// Current status value (mirrors the TLA+ `status` variable).
    pub status: String,
    /// Number of items currently in the entity (simplified from the TLA+ set).
    pub item_count: usize,
}

impl fmt::Display for TemperModelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(items={})", self.status, self.item_count)
    }
}

/// An action that the model can take, corresponding to a TLA+ transition.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TemperModelAction {
    /// The transition name (e.g. "SubmitOrder", "CancelOrder").
    pub name: String,
    /// The target status after taking this action (if deterministic).
    pub target_state: Option<String>,
}

impl fmt::Display for TemperModelAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.target_state {
            Some(target) => write!(f, "{} -> {}", self.name, target),
            None => write!(f, "{}", self.name),
        }
    }
}

/// A resolved transition used internally by the model, pre-computed from a
/// TLA+ `Transition` for efficient matching during state exploration.
#[derive(Clone, Debug)]
pub struct ResolvedTransition {
    /// The action name.
    pub name: String,
    /// States from which this transition can fire.
    pub from_states: Vec<String>,
    /// The target state (if deterministic).
    pub to_state: Option<String>,
    /// Whether this transition modifies the item count.
    pub modifies_items: bool,
    /// Whether this is an "add item" action (increments counter).
    pub is_add_item: bool,
    /// Whether this transition requires item_count > 0 to fire.
    pub requires_items: bool,
}

/// The kind of check an invariant performs.
#[derive(Clone, Debug)]
pub enum InvariantKind {
    /// status must be in a known set of states.
    StatusInSet,
    /// When status is in trigger_states, item_count must be > 0.
    ItemCountPositive,
    /// When status is in trigger_states, status must also be in required_states.
    Implication,
}

/// A safety invariant resolved for runtime checking.
#[derive(Clone, Debug)]
pub struct ResolvedInvariant {
    /// The invariant name.
    pub name: String,
    /// States in which this invariant's check is activated (empty = always).
    pub trigger_states: Vec<String>,
    /// For implication invariants: the set of valid target states.
    pub required_states: Vec<String>,
    /// The kind of check this invariant performs.
    pub kind: InvariantKind,
}

/// The Stateright model generated from a TLA+ `StateMachine`.
///
/// This struct holds all the pre-computed transition and invariant data needed
/// to implement the `Model` trait efficiently. Invariant data is stored here
/// (rather than captured in closures) because Stateright's `Property::always`
/// requires a bare `fn` pointer.
#[derive(Clone)]
pub struct TemperModel {
    /// All valid status values from the specification.
    pub states: Vec<String>,
    /// Pre-resolved transitions.
    pub transitions: Vec<ResolvedTransition>,
    /// Pre-resolved safety invariants (accessible to property fn pointers via &self).
    pub invariants: Vec<ResolvedInvariant>,
    /// The initial status (first state from Init, typically "Draft").
    pub(crate) initial_status: String,
    /// Maximum item count for bounded exploration.
    pub(crate) max_items: usize,
}
