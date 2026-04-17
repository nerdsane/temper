//! I/O Automaton types — the specification data model.
//!
//! Based on Lynch-Tuttle I/O Automata: a labeled state transition system
//! where each action has a precondition (predicate on pre-state) and an
//! effect (state change program).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::field_invariant::FieldInvariant;

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
    /// Context entity declarations for Cedar authorization.
    #[serde(default, rename = "context_entity")]
    pub context_entities: Vec<ContextEntityDecl>,
    /// Agent trigger declarations (auto-spawn agents on state transitions).
    #[serde(default, rename = "agent_trigger")]
    pub agent_triggers: Vec<AgentTrigger>,
    /// Cross-field validation rules evaluated on OData `POST`/`PATCH`.
    #[serde(default, rename = "field_invariant")]
    pub field_invariants: Vec<FieldInvariant>,
    /// State-entry timeouts (ADR-0049). Each entry declares that entering
    /// `state` arms a timer that fires `on_timeout` after `after_seconds`
    /// unless the entity leaves the state or a `reset_on` action fires.
    #[serde(default, rename = "state_timeout")]
    pub state_timeouts: Vec<StateTimeout>,
    /// Admission control caps (ADR-0051). When present, the dispatch layer
    /// gates concurrent calls per `(tenant, entity_type, action)` before
    /// reaching the actor.
    #[serde(default)]
    pub admission: Option<Admission>,
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
    /// States that are permitted to be indefinite (no `[[state_timeout]]`
    /// declaration required). Used by ADR-0050's liveness rule. Each entry
    /// must be a declared state name. Convention: authors add a nearby
    /// `# justification:` comment explaining why the state is indefinite.
    #[serde(default)]
    pub allow_indefinite_states: Vec<String>,
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
    /// Optional per-field inline ceiling in bytes for the field-overflow
    /// primitive (ADR-0045). Values above this size are moved to the blob
    /// store; values at or below stay inline in `fields`. When `None`, the
    /// crate-wide `DEFAULT_FIELD_INLINE_MAX` applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_inline_max_bytes: Option<usize>,
    /// Optional per-field TTL in seconds for overflow blobs (ADR-0047). When
    /// `None`, overflow blobs are permanent (match pre-ADR behavior). When
    /// set, the blob's `expires_at` is written as `datetime('now', '+N s')`
    /// and the sweeper deletes rows past their expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_ttl_seconds: Option<u64>,
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
    /// A boolean variable must be false.
    #[serde(rename = "is_false")]
    IsFalse { var: String },
    /// A list variable must contain a specific value.
    #[serde(rename = "list_contains")]
    ListContains { var: String, value: String },
    /// A list variable must have at least N elements.
    #[serde(rename = "list_length_min")]
    ListLengthMin { var: String, min: usize },
    /// Another entity must be in one of the required statuses.
    #[serde(rename = "cross_entity_state")]
    CrossEntityState {
        /// The target entity type (e.g., "TestWorkflow").
        entity_type: String,
        /// Field name on the current entity holding the target entity ID.
        entity_id_source: String,
        /// Target must be in one of these statuses (any match passes).
        required_status: Vec<String>,
    },
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
    /// Schedule an action at an absolute timestamp read from an entity field.
    #[serde(rename = "schedule_at")]
    ScheduleAt { action: String, field: String },
    /// Spawn a child entity as a post-transition effect.
    #[serde(rename = "spawn")]
    Spawn {
        /// The child entity type to create.
        entity_type: String,
        /// Source for the child entity ID: field name from params, or "{uuid}" for auto-generated.
        entity_id_source: String,
        /// Optional action to dispatch on the child after creation.
        initial_action: Option<String>,
        /// Optional field on the parent to store the child's ID.
        store_id_in: Option<String>,
        /// Optional list of field names to copy from parent state into child's initial_action params.
        copy_fields: Option<Vec<String>>,
    },
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

/// A context entity declaration for Cedar authorization.
///
/// Declares that another entity's status should be available in the Cedar
/// authorization context when evaluating policies for this entity type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEntityDecl {
    /// Label for this context entity (e.g., "parent_agent").
    pub name: String,
    /// The target entity type to look up (e.g., "LeadAgent").
    pub entity_type: String,
    /// Field on this entity holding the target entity's ID.
    pub id_field: String,
}

/// An agent trigger declaration.
///
/// When the specified action fires (optionally reaching a target state),
/// an Agent entity is auto-spawned and assigned the given role, goal, and
/// model. At registration time, these are synthesized into ReactionRules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTrigger {
    /// Trigger name (e.g., "test_on_ready").
    pub name: String,
    /// Action name that triggers agent spawning (e.g., "MarkReady").
    pub on_action: String,
    /// Optional target state filter (e.g., "Ready"). If set, the trigger
    /// only fires when the action transitions to this state.
    #[serde(default)]
    pub to_state: Option<String>,
    /// Role for the spawned agent.
    pub agent_role: String,
    /// Goal template for the spawned agent. May contain `${field}` placeholders
    /// that are resolved from the source entity's fields.
    pub agent_goal: String,
    /// Optional LLM model override for the spawned agent.
    #[serde(default)]
    pub agent_model: Option<String>,
    /// Optional AgentType ID for the spawned agent.
    #[serde(default)]
    pub agent_type_id: Option<String>,
}

/// A state-entry timeout declaration (ADR-0049).
///
/// Declares that entering `state` should schedule `on_timeout` to fire
/// after `after_seconds`. If the entity leaves `state` before the timer
/// fires, the timer is cancelled. If any action listed in `reset_on`
/// fires while the entity is in `state`, the timer is re-armed from now.
///
/// Authors write the declaration once; the spec compiler (see
/// `metadata::validate_state_timeouts` and the durable scheduler) generates
/// the supporting state variables (`{state}_entered_at`, `{state}_timeout_seq`)
/// and wires `state` into the target action's `from` list if it is not
/// already present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateTimeout {
    /// The state whose entry arms the timer. Must be a declared state.
    pub state: String,
    /// Wall-clock delay before `on_timeout` fires, in seconds.
    pub after_seconds: u64,
    /// Action to dispatch when the timer fires. Must be a declared action.
    pub on_timeout: String,
    /// Maximum times the timer can fire across repeated entries into `state`.
    /// Defaults to 1. Set higher for states entered multiple times where
    /// each entry should receive its own budget (e.g., `Recovering` with
    /// `max_occurrences = 3`).
    #[serde(default = "default_one")]
    pub max_occurrences: u32,
    /// Actions that, when fired while in `state`, re-arm the timer from
    /// the current moment. Progress signals such as `Heartbeat` go here.
    #[serde(default)]
    pub reset_on: Vec<String>,
    /// Params applied to the `on_timeout` action when it fires. Typically
    /// includes an `error_message` field for observability.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

fn default_one() -> u32 {
    1
}

/// Admission control declaration (ADR-0051).
///
/// Declared as a `[admission]` block inside the top-level entity spec:
///
/// ```toml
/// [admission]
/// max_concurrent_creates = 5
/// max_concurrent_actions = { "Submit" = 3 }
/// queue_depth = 50
/// queue_timeout_seconds = 30
/// ```
///
/// All fields are optional. A missing admission block means no gating for
/// that entity type (backward compatible).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Admission {
    /// Max concurrent pending `Create` (entity-instantiation) calls per
    /// tenant. `None` = unlimited.
    #[serde(default)]
    pub max_concurrent_creates: Option<u32>,
    /// Per-action caps. Key is the action name. Values are max-concurrent
    /// permits per tenant.
    #[serde(default)]
    pub max_concurrent_actions: BTreeMap<String, u32>,
    /// Max pending acquirers before new acquisitions are rejected with
    /// `Deferred`. Defaults to 100 when admission is configured at all.
    #[serde(default)]
    pub queue_depth: Option<u32>,
    /// Max wait an acquirer tolerates before `Deferred` is returned.
    /// Defaults to 30 seconds when admission is configured.
    #[serde(default)]
    pub queue_timeout_seconds: Option<u32>,
}

#[cfg(test)]
#[path = "types_test.rs"]
mod tests;
