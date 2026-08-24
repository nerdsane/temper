//! I/O Automaton types — the specification data model.
//!
//! Based on Lynch-Tuttle I/O Automata: a labeled state transition system
//! where each action has a precondition (predicate on pre-state) and an
//! effect (state change program).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::field_invariant::FieldInvariant;

/// Return whether a field name is owned by the runtime rather than an action.
///
/// Entity identity, lifecycle status, spec-governance metadata, and declared
/// context statuses are derived from server-proven state. Specs and callers
/// must not create a second mutable representation of these values.
pub fn is_server_derived_field_name(name: &str) -> bool {
    matches!(
        name,
        "Id" | "id"
            | "Status"
            | "status"
            | "has_spec"
            | "HasSpec"
            | "_temper_state_timeout_declaration_v1"
    ) || is_server_derived_context_status_name(name)
}

/// Return whether a field is in the server-derived context-status namespace.
pub fn is_server_derived_context_status_name(name: &str) -> bool {
    name.starts_with("ctx_") && name.ends_with("_status")
}

/// A complete I/O Automaton specification for a single entity type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Automaton {
    /// Automaton metadata.
    pub automaton: AutomatonMeta,
    /// State variable declarations.
    #[serde(default)]
    pub state: Vec<StateVar>,
    /// All actions (input, output, internal, Composite).
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
    /// Cross-field validation rules evaluated on OData `POST`/`PATCH`.
    #[serde(default, rename = "field_invariant")]
    pub field_invariants: Vec<FieldInvariant>,
    /// State-entry timeouts (ADR-0049). Each entry declares that entering
    /// `state` arms a timer that fires `on_timeout` after `after_seconds`
    /// unless the entity leaves the state or a `reset_on` action fires.
    #[serde(default, rename = "state_timeout")]
    pub state_timeouts: Vec<StateTimeout>,
    /// ADR-0153: declared unique keys (alternate keys). Each names a property
    /// set guaranteed unique across entities of this type; the kernel maintains
    /// a keyed index (`entity_key_index`) over it for O(log n) present/absent
    /// reads — the negative-existence access path. The OData alternate-key
    /// annotation is derived from this declaration.
    #[serde(default, rename = "key")]
    pub keys: Vec<KeyDecl>,
    /// ADR-0155: declared vector access paths. Each names a float-vector property
    /// (and the model-tag property that partitions its space) that the kernel
    /// indexes in `entity_vector_index` for exact-scan kNN reads (`Temper.Nearest`).
    /// Empty when the spec declared no `[[vector]]`.
    #[serde(default, rename = "vector")]
    pub vectors: Vec<VectorDecl>,
    /// Admission control caps (ADR-0051). When present, the dispatch layer
    /// gates concurrent calls per `(tenant, entity_type, action)` before
    /// reaching the actor.
    #[serde(default)]
    pub admission: Option<Admission>,
}

/// ADR-0153: a declared unique key (alternate key) on an entity. `properties`
/// is the set of state/field names whose combined values uniquely identify one
/// entity. The kernel maintains `entity_key_index` over each declared key for
/// O(log n) present/absent reads; the canonical key hash uses `properties` in
/// declared order. Multiple keys on one entity are distinguished by `name`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyDecl {
    /// Identifier for this key (the `key_name` in `entity_key_index`).
    pub name: String,
    /// The property set that is unique, in canonical (hash) order.
    pub properties: Vec<String>,
    /// Whether this key defines the entity's deterministic ID (ADR-0156).
    #[serde(default)]
    pub entity_id: bool,
}

/// ADR-0155: a declared vector access path on an entity. `property` names the
/// float-vector state variable (a JSON array, or a JSON-encoded string, of
/// `dims` floats); `model_property` names the state variable holding the model
/// tag that partitions the space (only vectors sharing a tag are ever compared).
/// The kernel maintains `entity_vector_index` over each declared path and serves
/// exact-scan kNN through `Temper.Nearest`. `metric` is one of `cosine`, `dot`,
/// `l2`. Multiple vector paths on one entity are distinguished by `name`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VectorDecl {
    /// Identifier for this path (the `decl_name` in `entity_vector_index` and the
    /// `decl=` argument to `Temper.Nearest`).
    pub name: String,
    /// The state variable holding the float vector to index.
    pub property: String,
    /// The state variable holding the model tag that partitions the vector space.
    pub model_property: String,
    /// Vector dimensionality. Must be > 0; a row whose parsed vector length differs
    /// is not indexed (same posture as an incomplete declared key).
    pub dims: usize,
    /// Similarity metric: `cosine`, `dot`, or `l2`.
    pub metric: String,
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
    /// Type: "status", "counter", "set", "string", "bool", or "ref".
    #[serde(rename = "type")]
    pub var_type: String,
    /// Target entity type when `var_type = "ref"` (ADR-0156).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
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
    /// Optional query-plane override. When `Some(false)`, the field remains in
    /// entity state but is omitted from the durable field index used for OData
    /// collection filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_indexed: Option<bool>,
}

/// A parameter on an action — either a plain name (defaults to string type)
/// or a typed declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ActionParam {
    Named(String),
    Typed {
        name: String,
        #[serde(rename = "type", default = "default_param_type")]
        param_type: String,
        /// Target entity type when `param_type = "ref"` (ADR-0156).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entity_type: Option<String>,
    },
}

fn default_param_type() -> String {
    "string".to_string()
}

impl ActionParam {
    pub fn name(&self) -> &str {
        match self {
            Self::Named(n) => n,
            Self::Typed { name, .. } => name,
        }
    }
    pub fn param_type(&self) -> &str {
        match self {
            Self::Named(_) => "string",
            Self::Typed { param_type, .. } => param_type,
        }
    }

    /// Return the declared target entity type for a typed reference parameter.
    pub fn entity_type(&self) -> Option<&str> {
        match self {
            Self::Named(_) => None,
            Self::Typed { entity_type, .. } => entity_type.as_deref(),
        }
    }
}

/// An action in the I/O Automaton.
///
/// Actions are classified by `kind`:
/// - `input`: arrives from the environment (HTTP request), always enabled
/// - `output`: emitted to the environment (event to Postgres, span to ClickHouse)
/// - `internal`: private state transition (the state machine step)
/// - `Composite`: one governed intent that may produce declared sub-writes
///
/// Each action has a precondition (guard) and effects (state changes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    /// Action name (e.g., "SubmitOrder").
    pub name: String,
    /// Action kind: "input", "output", "internal", or "Composite".
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
    pub params: Vec<ActionParam>,
    /// Agent hint for this action.
    pub hint: Option<String>,
    /// Whether a Composite action records an audit/idempotency event on the parent stream.
    #[serde(default = "default_record_parent_event")]
    pub record_parent_event: bool,
    /// Outgoing triggers fired post-commit of this action (ADR-0046).
    ///
    /// Each trigger describes one cross-entity dispatch, WASM module
    /// invocation, or webhook call. Triggers fire after the source action's
    /// transition commits (fire-and-forget — failures do not roll back the
    /// source). Kind-specific fields are validated at parse time.
    #[serde(default, rename = "triggers")]
    pub triggers: Vec<ActionTrigger>,
    /// Composite-action Cedar gate declaration (ADR-0040).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cedar_gate: Option<CompositeCedarGate>,
    /// Declared sub-write contract for Composite actions (ADR-0040).
    #[serde(default, rename = "sub_writes")]
    pub sub_writes: Vec<SubWriteSpec>,
}

fn default_internal() -> String {
    "internal".to_string()
}

fn default_record_parent_event() -> bool {
    true
}

/// The single Cedar gate evaluated for a Composite action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompositeCedarGate {
    /// Principal expression, usually `request.principal`.
    pub principal: String,
    /// Resource expression, usually `this`.
    pub resource: String,
    /// Cedar action identifier for the composite intent.
    pub action: String,
}

/// Declared write shape emitted by a Composite action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubWriteSpec {
    /// Target entity type receiving the write.
    pub target_entity: String,
    /// Target action, e.g. `Create` or `Update`.
    pub action: String,
    /// Handler input or source that generates this write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_from: Option<String>,
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
    /// A cross-entity status precondition on a *related* entity.
    ///
    /// Combines an allowlist (`required_status`) and a denylist
    /// (`forbidden_status`). For a *present, resolvable* target the guard holds
    /// iff the target's status is allowed AND not forbidden:
    /// - `required_status` empty ⇒ no allowlist constraint (any status allowed);
    ///   non-empty ⇒ the status must be one of them.
    /// - `forbidden_status` empty ⇒ no denylist constraint; non-empty ⇒ the
    ///   status must NOT be one of them.
    ///
    /// A denylist expresses "reject only when the container is in a *specific*
    /// bad state" without having to enumerate every good state — e.g. a write
    /// is refused only when its owning Workspace is `Frozen`/`Archived`, while a
    /// missing or not-yet-resolved Workspace still allows the write (see
    /// `required` for the empty/missing-ref semantics).
    #[serde(rename = "cross_entity_state")]
    CrossEntityState {
        /// The target entity type (e.g., "TestWorkflow").
        entity_type: String,
        /// Field name on the current entity holding the target entity ID.
        entity_id_source: String,
        /// Allowlist: target must be in one of these statuses (any match
        /// passes). Empty ⇒ no allowlist constraint.
        #[serde(default)]
        required_status: Vec<String>,
        /// Denylist: target must NOT be in any of these statuses. Empty ⇒ no
        /// denylist constraint.
        #[serde(default)]
        forbidden_status: Vec<String>,
        /// Whether the `entity_id_source` ref must be present (ARN-92 #2).
        ///
        /// When `false` (default), an empty/missing scalar ref or an empty list
        /// relation passes the guard vacuously — the legacy blast radius. When
        /// `true`, an empty/missing scalar or empty list ref *fails* the guard:
        /// a required relationship that was never set cannot satisfy a
        /// cross-entity status precondition.
        #[serde(default)]
        required: bool,
    },
    /// An incoming typed reference must equal a stored typed reference.
    #[serde(rename = "reference_equals")]
    ReferenceEquals {
        /// Stored typed-reference state variable.
        reference: String,
        /// Incoming typed-reference action parameter.
        param: String,
    },
}

/// An effect (state change in the post-state).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Effect {
    /// Increment a counter variable.
    #[serde(rename = "increment")]
    Increment {
        var: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amount: Option<String>,
    },
    /// Decrement a counter variable.
    #[serde(rename = "decrement")]
    Decrement {
        var: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amount: Option<String>,
    },
    /// Set a counter variable from an action param.
    #[serde(rename = "set_counter_from_param")]
    SetCounterFromParam { var: String, param: String },
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
    /// Marks this integration as an LLM call. The dispatcher upgrades matching
    /// spans to an LLM-kind root span so `gen_ai.*` content surfaces correctly
    /// in observability backends (e.g., Datadog LLM Obs). Defaults to false.
    #[serde(default)]
    pub llm: bool,
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

// ADR-0046: `AgentTrigger` struct and `[[agent_trigger]]` parser section
// removed. Agent spawning is now an `[[action.triggers]]` block with
// kind = "entity" targeting an agent entity (platform Agent, paw-agent
// Agent, or any app-defined agent). Auto-start-on-Assign behavior moves
// to the target agent's own spec as a self-trigger — see ADR-0046
// Sub-Decision 7.

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

// ─── ADR-0046: Unified Action Triggers ─────────────────────────────────────

/// Maximum cascade depth for recursive trigger dispatch (TigerStyle budget).
pub const MAX_TRIGGER_DEPTH: u32 = 8;

/// Maximum nesting depth for composite trigger guards (AllOf/AnyOf/Not).
pub const MAX_TRIGGER_GUARD_DEPTH: u32 = 4;

/// Maximum number of triggers a tenant can register across all entities
/// (TigerStyle budget).
pub const MAX_TRIGGERS_PER_TENANT: usize = 1024;

/// Kind of trigger: what kind of outgoing effect this trigger produces.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TriggerKind {
    /// Cross-entity action dispatch. Target is another entity's action.
    Entity,
    /// WASM module execution. Optionally dispatches `on_success` / `on_failure`
    /// actions on the source entity afterwards.
    Wasm,
    /// Native platform adapter execution. Optionally dispatches `on_success`
    /// / `on_failure` actions on the source entity afterwards.
    Adapter,
    /// Outbound HTTP webhook. Optionally dispatches `on_success` / `on_failure`
    /// actions on the source entity afterwards.
    Webhook,
}

/// Liveness expectation for a trigger (ADR-0046, minimal hook).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerLiveness {
    /// No liveness expectation. Verifier does not emit eventually-properties.
    None,
    /// Best-effort (default). Dispatcher attempts the trigger; liveness is not
    /// asserted by the verifier.
    #[default]
    BestEffort,
    /// Required. The composite verifier emits a `Property::eventually` that
    /// the target action (entity kind) or on_success action (wasm/webhook
    /// kinds) fires following the source action. Assumes weakly-fair dispatch.
    Required,
}

/// How to resolve the target entity ID for a `kind = "entity"` trigger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetResolver {
    /// Read the target entity ID from a field on the source entity.
    Field {
        /// Field name on the source entity containing the target ID.
        field: String,
    },
    /// Use the source entity's own ID as the target ID (self-trigger or
    /// same-ID cross-entity dispatch).
    SameId,
    /// Use a fixed entity ID literal.
    Static {
        /// The fixed entity ID to target.
        entity_id: String,
    },
    /// Read target ID from `id_field`; create the target if it does not exist.
    CreateIfMissing {
        /// Field name containing (or to receive) the target ID.
        id_field: String,
    },
    /// Generate a fresh `sim_uuid()` for each trigger dispatch. Correct
    /// resolver for pipeline chaining where each source commit spawns a new
    /// target instance. `sim_uuid()` (not `Uuid::new_v4`) is required for DST
    /// compliance across seeded runs.
    Create,
}

/// Conditional firing predicate for a trigger.
///
/// Evaluated post-commit against the source entity's post-action fields
/// (sync variants) or another entity's current state (`CrossEntityStateIn`).
/// Guard-skipped triggers do not emit a dispatch record — they never fired.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerGuard {
    /// Source field equals the given JSON value.
    FieldEquals {
        /// Field name on the source entity.
        field: String,
        /// Expected value (string, number, bool, null).
        value: serde_json::Value,
    },
    /// Source field is one of the given JSON values.
    FieldIn {
        /// Field name on the source entity.
        field: String,
        /// Allowed values.
        values: Vec<serde_json::Value>,
    },
    /// Source field is a JSON boolean `true`.
    BoolTrue {
        /// Field name on the source entity.
        field: String,
    },
    /// Source field is a JSON boolean `false`.
    BoolFalse {
        /// Field name on the source entity.
        field: String,
    },
    /// Source entity's post-action status is one of the given values.
    StateIn {
        /// Allowed states.
        values: Vec<String>,
    },
    /// Another entity's current status must be one of the given values.
    CrossEntityStateIn {
        /// Target entity type.
        entity_type: String,
        /// Source-entity field name holding the target entity id.
        entity_id_source: String,
        /// Target entity statuses that satisfy the guard.
        required_status: Vec<String>,
    },
    /// All child guards must pass. Bounded by `MAX_TRIGGER_GUARD_DEPTH`.
    AllOf {
        /// Child guards.
        guards: Vec<TriggerGuard>,
    },
    /// At least one child guard must pass. Bounded by `MAX_TRIGGER_GUARD_DEPTH`.
    AnyOf {
        /// Child guards.
        guards: Vec<TriggerGuard>,
    },
    /// Inverted child guard. Bounded by `MAX_TRIGGER_GUARD_DEPTH`.
    Not {
        /// Inner guard.
        guard: Box<TriggerGuard>,
    },
}

impl TriggerGuard {
    /// Compute the maximum composite nesting depth of this guard.
    ///
    /// Leaf variants are depth 1. `AllOf` / `AnyOf` / `Not` add one level.
    /// Used at parse time to reject guards deeper than
    /// `MAX_TRIGGER_GUARD_DEPTH`.
    pub fn depth(&self) -> u32 {
        match self {
            TriggerGuard::FieldEquals { .. }
            | TriggerGuard::FieldIn { .. }
            | TriggerGuard::BoolTrue { .. }
            | TriggerGuard::BoolFalse { .. }
            | TriggerGuard::StateIn { .. }
            | TriggerGuard::CrossEntityStateIn { .. } => 1,
            TriggerGuard::AllOf { guards } | TriggerGuard::AnyOf { guards } => {
                1 + guards.iter().map(Self::depth).max().unwrap_or(0)
            }
            TriggerGuard::Not { guard } => 1 + guard.depth(),
        }
    }
}

/// An outgoing trigger declared inline on an `Action` (ADR-0046).
///
/// Unifies cross-entity dispatch (former `reactions.toml`), WASM execution
/// (former `[[integration]] type = "wasm"`), native adapters (former
/// `[[integration]] type = "adapter"`), and outbound webhooks (former
/// `[[integration]] type = "webhook"`) under a single `kind`-discriminated
/// schema.
///
/// Fields are a superset across kinds; parse-time validation enforces
/// presence per `kind`:
/// - `Entity`: requires `target_entity` + `target_action`.
/// - `Wasm`: requires `module`.
/// - `Adapter`: requires `adapter` or `adapter_type`.
/// - `Webhook`: requires `url` + `method`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionTrigger {
    /// Human-readable name for logging and debugging.
    pub name: String,
    /// Discriminator — determines which field set applies.
    pub kind: TriggerKind,
    /// Optional principal (names a registered `AgentType`). When `None`, the
    /// trigger inherits the invoking principal's `SecurityContext`. When
    /// `Some`, a synthetic `SecurityContext` is built with every Cedar
    /// attribute (`role`, `agent_type`, `id`) populated from this name.
    /// Parse-time verification fails if the named `AgentType` is not
    /// registered in the tenant.
    #[serde(default)]
    pub principal: Option<String>,
    /// Only fire when the source action transitions to this state. `None` =
    /// fire on any outcome.
    #[serde(default)]
    pub to_state: Option<String>,
    /// Optional firing predicate evaluated post-commit.
    #[serde(default)]
    pub guard: Option<TriggerGuard>,
    /// Liveness expectation (see `TriggerLiveness`).
    #[serde(default)]
    pub liveness: TriggerLiveness,
    /// Marks this reaction as an intentional best-effort drop (ADR-0150).
    ///
    /// When `true`, the composite verifier's `no_dropped_reaction` property
    /// does not flag this trigger as a violation if its target action is not
    /// enabled from the target's current state. Use it for reactions that are
    /// *meant* to be skipped when the target is not ready (e.g. a notification
    /// that is meaningless once an entity is archived). Defaults to `false`:
    /// a dropped reaction is treated as a bug unless explicitly opted out.
    #[serde(default)]
    pub drop_ok: bool,
    /// Marks this trigger as an LLM call for observability. The dispatcher
    /// promotes matching spans to LLM-kind root spans so `gen_ai.*` content
    /// surfaces correctly in observability backends (e.g., Datadog LLM Obs).
    #[serde(default)]
    pub llm: bool,

    // ─── Entity-kind fields ─────────────────────────────────────────────
    /// Target entity type (required for `Entity` kind).
    #[serde(default)]
    pub target_entity: Option<String>,
    /// Target action to dispatch (required for `Entity` kind).
    #[serde(default)]
    pub target_action: Option<String>,
    /// Static parameters to pass to the target action.
    #[serde(default)]
    pub params: serde_json::Value,
    /// Dynamic params: target-param-name → source-entity-field-name.
    ///
    /// At dispatch time, each key is read from the source entity's fields and
    /// merged with `params`. Collisions between `params` and `params_from`
    /// keys are rejected at parse time. A missing source field logs a warning
    /// and skips the key.
    #[serde(default)]
    pub params_from: BTreeMap<String, String>,
    /// How to find the target entity ID (required for `Entity` kind).
    #[serde(default)]
    pub resolve_target: Option<TargetResolver>,

    // ─── Wasm-kind fields ───────────────────────────────────────────────
    /// WASM module name (required for `Wasm` kind).
    #[serde(default)]
    pub module: Option<String>,
    /// Action to dispatch on the source entity on successful module
    /// execution.
    #[serde(default)]
    pub on_success: Option<String>,
    /// Action to dispatch on the source entity on failed module execution.
    #[serde(default)]
    pub on_failure: Option<String>,
    /// Arbitrary config passed to the WASM module at invocation time.
    /// Common keys: `url`, `method`, `headers`, `api_key_ref`.
    #[serde(default)]
    pub config: BTreeMap<String, String>,

    // ─── Adapter-kind fields ───────────────────────────────────────────
    /// Native adapter key (required for `Adapter` kind unless
    /// `adapter_type` is present).
    #[serde(default)]
    pub adapter: Option<String>,
    /// Alternate native adapter key name accepted by the runtime dispatcher.
    #[serde(default)]
    pub adapter_type: Option<String>,

    // ─── Webhook-kind fields ────────────────────────────────────────────
    /// Outbound HTTP URL (required for `Webhook` kind).
    #[serde(default)]
    pub url: Option<String>,
    /// HTTP method (required for `Webhook` kind — typically POST/PUT/PATCH).
    #[serde(default)]
    pub method: Option<String>,
    /// HTTP headers. Values may contain `{secret:key}` templates resolved
    /// from tenant-scoped secret storage.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Template for the HTTP request body. `${field}` placeholders are
    /// resolved from the source entity's post-action fields.
    #[serde(default)]
    pub body_template: Option<String>,
}

#[cfg(test)]
#[path = "types_test.rs"]
mod tests;
