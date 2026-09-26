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
    /// Must hold after every successful transition.
    pub assert: temper_spec::predicate::Expr,
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

    /// States from the spec's `terminal` list. A successful transition from
    /// one terminal state to another is a violation. Returns empty by default.
    fn terminal_states(&self) -> &[String] {
        &[]
    }

    /// A state variable or field by name, for invariant evaluation (counters
    /// as numbers, booleans, lists as arrays). `None` reads as absent.
    /// Default: no state.
    fn state_value(&self, _name: &str) -> Option<serde_json::Value> {
        None
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
