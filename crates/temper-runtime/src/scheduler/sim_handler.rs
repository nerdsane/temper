//! Type-erased actor handler for deterministic simulation.
//!
//! [`SimActorHandler`] is the trait that entity actors implement to participate
//! in simulation. It provides synchronous `init()` and `handle_message()`
//! methods — no async, no tokio, no persistence, no telemetry. Just the
//! core state machine logic (same `TransitionTable::evaluate()` call as
//! production).

/// A safety invariant derived from the I/O Automaton spec's `[[invariant]]` sections.
///
/// The simulation system checks these after every successful transition,
/// removing the need for callers to manually duplicate invariant logic.
#[derive(Debug, Clone)]
pub struct SpecInvariant {
    /// Invariant name (e.g., "SubmitRequiresItems").
    pub name: String,
    /// States in which this invariant is checked. Empty means all states.
    pub when: Vec<String>,
    /// The assertion expression from the spec.
    pub assert: SpecAssert,
}

/// A checkable assertion from the spec.
///
/// These map to the small set of invariant patterns that the framework can
/// check automatically. Domain-specific invariants that don't fit these
/// patterns still need a manual `set_invariant_checker()`.
#[derive(Debug, Clone)]
pub enum SpecAssert {
    /// A counter variable must be positive (e.g., `items > 0`).
    CounterPositive { var: String },
    /// The entity is in a terminal state — no further transitions allowed.
    NoFurtherTransitions,
    /// State A must have been visited before state B in event history.
    /// Expressed as: `ordering(A, B)` — "A precedes B".
    OrderingConstraint { before: String, after: String },
    /// The entity should never be in this state.
    /// Expressed as: `never(StateName)`.
    NeverState { state: String },
    /// A counter must satisfy a comparison (e.g., `items >= 1`, `retries < 5`).
    CounterCompare {
        var: String,
        op: CompareOp,
        value: usize,
    },
}

/// Comparison operators for counter invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareOp {
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Gte,
    /// Less than.
    Lt,
    /// Less than or equal.
    Lte,
    /// Equal.
    Eq,
}

/// A type-erased actor handler for simulation.
///
/// Implementors wrap a real `TransitionTable` and `EntityState` and
/// expose synchronous methods for the simulation to drive.
pub trait SimActorHandler: Send {
    /// Initialize the actor and return its initial state as JSON.
    fn init(&mut self) -> Result<serde_json::Value, String>;

    /// Handle a message (action + params) and return the resulting state.
    fn handle_message(&mut self, action: &str, params: &str) -> Result<serde_json::Value, String>;

    /// Current status string (e.g., "Draft", "Submitted").
    fn current_status(&self) -> String;

    /// Current item count.
    fn current_item_count(&self) -> usize;

    /// Total number of events recorded by this actor.
    fn event_count(&self) -> usize;

    /// Actions enabled from the current status.
    fn valid_actions(&self) -> Vec<String>;

    /// All recorded events as JSON array.
    fn events_json(&self) -> serde_json::Value;

    /// Invariants derived from the spec's `[[invariant]]` sections.
    ///
    /// Override this to expose invariants from the IOA spec. The
    /// [`SimActorSystem`] checks these automatically after every
    /// successful transition. Returns empty by default.
    fn spec_invariants(&self) -> &[SpecInvariant] {
        &[]
    }

    /// Custom effects (integration triggers) emitted by the last action.
    ///
    /// After each successful `handle_message()`, the simulation system calls
    /// this to discover WASM integration triggers. The system then schedules
    /// configured callback actions (success or failure) on the next tick.
    /// Returns empty by default (no integrations).
    fn pending_callbacks(&self) -> Vec<String> {
        Vec::new()
    }
}
