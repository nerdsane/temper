//! I/O Automaton types — the specification data model.
//!
//! Based on Lynch-Tuttle I/O Automata: a labeled state transition system
//! where each action has a precondition (predicate on pre-state) and an
//! effect (state change program).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// Liveness properties (something eventually happens).
    #[serde(default, rename = "liveness")]
    pub liveness: Vec<Liveness>,
    /// Integration declarations (external triggers).
    #[serde(default, rename = "integration")]
    pub integrations: Vec<Integration>,
    /// Inbound webhook declarations (external callback receivers).
    #[serde(default, rename = "webhook")]
    pub webhooks: Vec<Webhook>,
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
    /// A list variable must contain a specific value.
    #[serde(rename = "list_contains")]
    ListContains { var: String, value: String },
    /// A list variable must have at least N elements.
    #[serde(rename = "list_length_min")]
    ListLengthMin { var: String, min: usize },
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
    /// Append a value to a list variable (value comes from action params).
    #[serde(rename = "list_append")]
    ListAppend { var: String },
    /// Remove a value from a list variable by index (index from action params).
    #[serde(rename = "list_remove_at")]
    ListRemoveAt { var: String },
    /// Trigger a named WASM integration (post-transition async execution).
    #[serde(rename = "trigger")]
    Trigger { name: String },
    /// Schedule a delayed action on the same entity.
    #[serde(rename = "schedule")]
    Schedule { action: String, delay_seconds: u64 },
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

/// A liveness property.
///
/// Liveness properties assert that something "eventually happens" — a state
/// is eventually reached, or deadlock never occurs from certain states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Liveness {
    /// Property name.
    pub name: String,
    /// States from which this property is checked.
    #[serde(default)]
    pub from: Vec<String>,
    /// Target states that must eventually be reached.
    #[serde(default)]
    pub reaches: Vec<String>,
    /// If true, asserts that actions are always available (no deadlock).
    #[serde(default)]
    pub has_actions: Option<bool>,
}

/// An integration declaration (external system trigger).
///
/// Integrations declare that a state machine event should trigger an external
/// action (e.g., a webhook call or WASM module invocation). They are metadata
/// only — they do not affect state transitions or verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Integration {
    /// Integration name (e.g., "notify_fulfillment", "charge_payment").
    pub name: String,
    /// The event that triggers this integration (action name or trigger name).
    pub trigger: String,
    /// Integration type: "webhook" or "wasm".
    #[serde(rename = "type", default = "default_webhook")]
    pub integration_type: String,
    /// WASM module name (required when `type = "wasm"`).
    #[serde(default)]
    pub module: Option<String>,
    /// Action to dispatch on successful WASM execution (required when `type = "wasm"`).
    #[serde(default)]
    pub on_success: Option<String>,
    /// Action to dispatch on failed WASM execution (required when `type = "wasm"`).
    #[serde(default)]
    pub on_failure: Option<String>,
    /// Arbitrary config passed to the WASM module at invocation time.
    /// Common keys: `url`, `method`, `headers`.
    #[serde(flatten, default)]
    pub config: BTreeMap<String, String>,
}

fn default_webhook() -> String {
    "webhook".to_string()
}

/// Default method for webhooks.
fn default_post() -> String {
    "POST".to_string()
}

/// Default entity lookup strategy.
fn default_query_param() -> String {
    "query_param".to_string()
}

/// An inbound webhook declaration.
///
/// Webhooks allow external systems (OAuth providers, payment gateways) to
/// call back into Temper, triggering entity actions. They are metadata-only
/// — they do not affect verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    /// Webhook name (e.g., "oauth_callback").
    pub name: String,
    /// URL path suffix (e.g., "oauth/callback").
    pub path: String,
    /// HTTP method (default: POST).
    #[serde(default = "default_post")]
    pub method: String,
    /// Action to dispatch when webhook is called.
    pub action: String,
    /// How to find the target entity: "query_param", "body_field", "header", "path_param".
    #[serde(default = "default_query_param")]
    pub entity_lookup: String,
    /// Which parameter holds the entity ID.
    #[serde(default)]
    pub entity_param: Option<String>,
    /// Parameter extraction map (e.g., {"code": "query.code"}).
    #[serde(default)]
    pub extract: BTreeMap<String, String>,
    /// Optional HMAC secret for transport-layer validation (supports {secret:key} templates).
    #[serde(default)]
    pub hmac_secret: Option<String>,
    /// Header containing the HMAC signature from the external system.
    #[serde(default)]
    pub hmac_header: Option<String>,
}
