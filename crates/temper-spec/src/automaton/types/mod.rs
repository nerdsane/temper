//! I/O Automaton types — the specification data model.
//!
//! Based on Lynch-Tuttle I/O Automata: a labeled state transition system
//! where each action has a precondition (predicate on pre-state) and an
//! effect (state change program).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod action;
mod carry;
mod group;
mod state;
mod trigger;
mod webhook;

pub use action::*;
pub use carry::*;
pub use group::*;
pub use state::*;
pub use trigger::*;
pub use webhook::*;

use super::field_invariant::FieldInvariant;
use crate::predicate::{Arg, Expr};

/// Return whether a field name is owned by the runtime rather than an action.
///
/// Entity identity, lifecycle status, spec-governance metadata, and declared
/// context statuses are derived from server-proven state. Specs and callers
/// must not create a second mutable representation of these values.
pub fn is_server_derived_field_name(name: &str) -> bool {
    matches!(
        name,
        "Id" | "id" | "Status" | "status" | "has_spec" | "HasSpec"
    ) || is_event_metadata_field(name)
        || is_server_derived_context_status_name(name)
}

/// What the kernel keeps of a record's events, read at the top of the record
/// and never from an app's fields.
pub const EVENT_METADATA_FIELDS: [&str; 8] = [
    "first_event_at",
    "last_event_at",
    "last_event_via",
    "last_event_by",
    "last_event_for",
    "last_event_agent_type",
    "last_event_label",
    "timer_needs_person",
];

/// Whether `name` is one of the [`EVENT_METADATA_FIELDS`].
pub fn is_event_metadata_field(name: &str) -> bool {
    EVENT_METADATA_FIELDS.contains(&name)
}

/// Return whether a field is in the server-derived context-status namespace.
pub fn is_server_derived_context_status_name(name: &str) -> bool {
    name.starts_with("ctx_") && name.ends_with("_status")
}

/// A complete I/O Automaton specification for a single entity type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Automaton {
    /// Automaton metadata.
    pub automaton: AutomatonMeta,
    /// State variable declarations.
    #[serde(default)]
    pub state: Vec<StateVar>,
    /// All actions (input, output, internal, composite).
    #[serde(default, rename = "action")]
    pub actions: Vec<Action>,
    /// Safety invariants (must always hold).
    #[serde(default, rename = "invariant")]
    pub invariants: Vec<Invariant>,
    /// Rules every move into a place must meet as it fires.
    #[serde(default, rename = "entry_rule")]
    pub entry_rules: Vec<EntryRule>,
    /// Liveness properties (something eventually happens).
    #[serde(default, rename = "liveness")]
    pub liveness: Vec<Liveness>,
    /// Dispatch records derived from external `[[action.triggers]]` blocks.
    #[serde(default, skip_deserializing)]
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
    /// Where this version moves stored records it would otherwise leave with
    /// no way forward (see [`Carry`]).
    #[serde(default, rename = "carry")]
    pub carries: Vec<Carry>,
    /// Admission control caps (ADR-0051). When present, the dispatch layer
    /// gates concurrent calls per `(tenant, entity_type, action)` before
    /// reaching the actor.
    #[serde(default)]
    pub admission: Option<Admission>,
    /// State groups for drawing the spec as a statechart, flat and in source
    /// order; nesting is by name. Labels only.
    #[serde(default, rename = "group")]
    pub groups: Vec<Group>,
}

/// ADR-0153: a declared unique key (alternate key) on an entity. `properties`
/// is the set of state/field names whose combined values uniquely identify one
/// entity. The kernel maintains `entity_key_index` over each declared key for
/// O(log n) present/absent reads; the canonical key hash uses `properties` in
/// declared order. Multiple keys on one entity are distinguished by `name`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyDecl {
    /// Identifier for this key (the `key_name` in `entity_key_index`).
    pub name: String,
    /// The property set that is unique, in canonical (hash) order.
    pub properties: Vec<String>,
}

/// ADR-0155: a declared vector access path on an entity. `property` names the
/// float-vector state variable (a JSON array, or a JSON-encoded string, of
/// `dims` floats); `model_property` names the state variable holding the model
/// tag that partitions the space (only vectors sharing a tag are ever compared).
/// The kernel maintains `entity_vector_index` over each declared path and serves
/// exact-scan kNN through `Temper.Nearest`. `metric` is one of `cosine`, `dot`,
/// `l2`. Multiple vector paths on one entity are distinguished by `name`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    /// Restrict writes to declared action parameters and guarded actions.
    #[serde(default)]
    pub strict_action_params: bool,
    /// States no action may leave. The verifier checks that no action lists
    /// them in `from`.
    #[serde(default)]
    pub terminal: Vec<String>,
    /// A record of this type whose id is a user's `sub` describes that user:
    /// Cedar sees its status on the principal, as `principal.<Type>` when the
    /// user calls in person and `principal.actingFor<Type>` when an agent
    /// calls for them, read from the record's actor on every request.
    #[serde(default)]
    pub principal_record: bool,
}

/// A safety invariant, proven by the verification cascade.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invariant {
    /// Invariant name.
    pub name: String,
    /// Must hold in every reachable state.
    pub assert: Expr,
}

/// A rule on entering a place, proven by the verification cascade: every
/// move into `place` from one of `from` must have `assert` true in the state
/// just before it fires.
///
/// It says how a record may start something, which an invariant cannot: a
/// cup may start being made only while its topping is in stock, yet the
/// topping may run out while the cup is being made.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryRule {
    /// Rule name.
    pub name: String,
    /// The place the rule guards.
    pub place: String,
    /// The places whose moves into `place` the rule covers. Empty: every
    /// other place, so a move that stays in `place` is not covered.
    #[serde(default)]
    pub from: Vec<String>,
    /// Must hold just before each covered move fires. It reads what a guard
    /// reads, except the move's parameters: the rule covers several moves.
    pub assert: Expr,
}

/// A liveness property.
///
/// Liveness properties assert that something "eventually happens" — a state
/// is eventually reached, or deadlock never occurs from certain states.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Liveness {
    /// Property name.
    pub name: String,
    /// States from which this property is checked.
    #[serde(default)]
    pub from: Vec<String>,
    /// Target states that must eventually be reached.
    #[serde(default)]
    pub reaches: Vec<String>,
    /// Places a record in `from` (in any place when `from` is empty) can
    /// always still reach: from each such state, some moves lead to one of
    /// them. Unlike `reaches`, not every path has to get there; the way must
    /// never be closed off. Checked on the model check's whole walk.
    #[serde(default)]
    pub can_reach: Vec<String>,
    /// If true, asserts that actions are always available (no deadlock).
    #[serde(default)]
    pub has_actions: Option<bool>,
    /// A condition the records reach by themselves: from every reachable
    /// state, the moves the system makes (state timers, module answers,
    /// reactions, and the actions in `relies_on`) can bring them to a state
    /// with nothing in flight where it holds. It may read related records
    /// (`Type[id].field`), and is checked on their joint model.
    #[serde(default)]
    pub eventually: Option<Expr>,
    /// Actions of this entity that code outside the specs sends whenever
    /// `eventually` does not hold, such as a site that nudges a record it
    /// finds behind. The check counts them as moves the system makes while
    /// the condition is false.
    #[serde(default)]
    pub relies_on: Vec<String>,
}

/// A dispatch record the runtime's WASM and adapter dispatchers look up by
/// name. Not written in specs: derived at parse time from each external
/// `[[action.triggers]]` block (see `parse_automaton`). They do not affect
/// state transitions or verification.
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
    /// The actions a successful call may take, in the order the kernel tries
    /// them (see [`ActionTrigger::on_success`]).
    #[serde(default, with = "trigger::one_or_many")]
    pub on_success: Vec<String>,
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

/// A context entity declaration for Cedar authorization.
///
/// Declares that another entity's status should be available in the Cedar
/// authorization context when evaluating policies for this entity type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct StateTimeout {
    /// The state whose entry arms the timer. Must be a declared state.
    pub state: String,
    /// Wall-clock delay before `on_timeout` fires, in seconds.
    pub after_seconds: u64,
    /// Action to dispatch when the timer fires. Must be a declared action.
    pub on_timeout: String,
    /// Has no effect. The timer is armed on every entry into `state` and
    /// nothing counts how often it fired. The key is still accepted so specs
    /// that set it keep loading; spec lint warns
    /// `state_timeout_max_occurrences_ignored` when it is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_occurrences: Option<u32>,
    /// Actions that, when fired while in `state`, re-arm the timer from
    /// the current moment. Progress signals such as `Heartbeat` go here.
    #[serde(default)]
    pub reset_on: Vec<String>,
    /// Parameters passed to the `on_timeout` action, read when the timer is
    /// armed. Each value is a literal (`'text'`, an integer, `true`, `false`,
    /// `null`) or a declared state variable (`Id` is the entity's id).
    /// Typically includes an `error_message` for observability.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, Arg>,
    /// Words for people reading the app: what the timer does ("check CI
    /// again"). Display only: the kernel never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub says: Option<String>,
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
#[serde(deny_unknown_fields)]
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
#[path = "../types_test.rs"]
mod tests;
