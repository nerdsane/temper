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
    /// Context entity declarations for Cedar authorization.
    #[serde(default, rename = "context_entity")]
    pub context_entities: Vec<ContextEntityDecl>,
    /// Agent trigger declarations (auto-spawn agents on state transitions).
    #[serde(default, rename = "agent_trigger")]
    pub agent_triggers: Vec<AgentTrigger>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_automaton() {
        let toml_src = r#"
[automaton]
name = "Order"
states = ["Draft", "Active"]
initial = "Draft"
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        assert_eq!(a.automaton.name, "Order");
        assert_eq!(a.automaton.states, vec!["Draft", "Active"]);
        assert_eq!(a.automaton.initial, "Draft");
        assert!(a.actions.is_empty());
        assert!(a.invariants.is_empty());
        assert!(a.liveness.is_empty());
        assert!(a.integrations.is_empty());
    }

    #[test]
    fn parse_action_defaults() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[action]]
name = "DoIt"
from = ["A"]
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        assert_eq!(a.actions.len(), 1);
        assert_eq!(a.actions[0].kind, "internal");
        assert!(a.actions[0].to.is_none());
        assert!(a.actions[0].guard.is_empty());
        assert!(a.actions[0].effect.is_empty());
    }

    #[test]
    fn parse_guard_variants() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A", "B"]
initial = "A"

[[action]]
name = "G1"
from = ["A"]
to = "B"
guard = [
    { type = "min_count", var = "items", min = 1 },
    { type = "max_count", var = "items", max = 10 },
    { type = "is_true", var = "ready" },
    { type = "list_contains", var = "tags", value = "vip" },
    { type = "list_length_min", var = "tags", min = 2 },
]
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        let guards = &a.actions[0].guard;
        assert_eq!(guards.len(), 5);
        assert!(matches!(&guards[0], Guard::MinCount { var, min: 1 } if var == "items"));
        assert!(matches!(&guards[1], Guard::MaxCount { var, max: 10 } if var == "items"));
        assert!(matches!(&guards[2], Guard::IsTrue { var } if var == "ready"));
        assert!(
            matches!(&guards[3], Guard::ListContains { var, value } if var == "tags" && value == "vip")
        );
        assert!(matches!(&guards[4], Guard::ListLengthMin { var, min: 2 } if var == "tags"));
    }

    #[test]
    fn parse_effect_variants() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[action]]
name = "E1"
from = ["A"]
effect = [
    { type = "increment", var = "count" },
    { type = "decrement", var = "count" },
    { type = "set_bool", var = "done", value = true },
    { type = "emit", event = "order_placed" },
    { type = "list_append", var = "log" },
    { type = "list_remove_at", var = "log" },
    { type = "trigger", name = "run_wasm" },
    { type = "schedule", action = "Retry", delay_seconds = 30 },
]
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        let effects = &a.actions[0].effect;
        assert_eq!(effects.len(), 8);
        assert!(matches!(&effects[0], Effect::Increment { var } if var == "count"));
        assert!(matches!(&effects[1], Effect::Decrement { var } if var == "count"));
        assert!(matches!(&effects[2], Effect::SetBool { var, value: true } if var == "done"));
        assert!(matches!(&effects[3], Effect::Emit { event } if event == "order_placed"));
        assert!(matches!(&effects[4], Effect::ListAppend { var } if var == "log"));
        assert!(matches!(&effects[5], Effect::ListRemoveAt { var } if var == "log"));
        assert!(matches!(&effects[6], Effect::Trigger { name } if name == "run_wasm"));
        assert!(
            matches!(&effects[7], Effect::Schedule { action, delay_seconds: 30 } if action == "Retry")
        );
    }

    #[test]
    fn parse_spawn_effect() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[action]]
name = "S1"
from = ["A"]
effect = [
    { type = "spawn", entity_type = "Child", entity_id_source = "{uuid}", initial_action = "Init", store_id_in = "child_id" },
]
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        match &a.actions[0].effect[0] {
            Effect::Spawn {
                entity_type,
                entity_id_source,
                initial_action,
                store_id_in,
            } => {
                assert_eq!(entity_type, "Child");
                assert_eq!(entity_id_source, "{uuid}");
                assert_eq!(initial_action.as_deref(), Some("Init"));
                assert_eq!(store_id_in.as_deref(), Some("child_id"));
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn parse_invariant_and_liveness() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A", "B", "C"]
initial = "A"

[[invariant]]
name = "NonNeg"
when = ["B"]
assert = "count >= 0"

[[liveness]]
name = "Progress"
from = ["A"]
reaches = ["C"]
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        assert_eq!(a.invariants.len(), 1);
        assert_eq!(a.invariants[0].name, "NonNeg");
        assert_eq!(a.invariants[0].when, vec!["B"]);
        assert_eq!(a.invariants[0].assert, "count >= 0");

        assert_eq!(a.liveness.len(), 1);
        assert_eq!(a.liveness[0].name, "Progress");
        assert_eq!(a.liveness[0].from, vec!["A"]);
        assert_eq!(a.liveness[0].reaches, vec!["C"]);
    }

    #[test]
    fn parse_integration() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[integration]]
name = "payment"
trigger = "ChargeCard"
type = "wasm"
module = "payment_processor"
on_success = "PaymentConfirmed"
on_failure = "PaymentFailed"
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        assert_eq!(a.integrations.len(), 1);
        assert_eq!(a.integrations[0].name, "payment");
        assert_eq!(a.integrations[0].integration_type, "wasm");
        assert_eq!(
            a.integrations[0].module.as_deref(),
            Some("payment_processor")
        );
        assert_eq!(
            a.integrations[0].on_success.as_deref(),
            Some("PaymentConfirmed")
        );
    }

    #[test]
    fn parse_webhook() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[webhook]]
name = "oauth_cb"
path = "oauth/callback"
action = "HandleCallback"
entity_param = "state"
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        assert_eq!(a.webhooks.len(), 1);
        assert_eq!(a.webhooks[0].name, "oauth_cb");
        assert_eq!(a.webhooks[0].method, "POST"); // default
        assert_eq!(a.webhooks[0].entity_lookup, "query_param"); // default
    }

    #[test]
    fn parse_cross_entity_guard() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Act"
from = ["A"]
to = "B"
guard = [{ type = "cross_entity_state", entity_type = "Parent", entity_id_source = "parent_id", required_status = ["Done", "Approved"] }]
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        match &a.actions[0].guard[0] {
            Guard::CrossEntityState {
                entity_type,
                entity_id_source,
                required_status,
            } => {
                assert_eq!(entity_type, "Parent");
                assert_eq!(entity_id_source, "parent_id");
                assert_eq!(
                    required_status,
                    &vec!["Done".to_string(), "Approved".to_string()]
                );
            }
            other => panic!("expected CrossEntityState, got {other:?}"),
        }
    }

    #[test]
    fn parse_context_entity() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[context_entity]]
name = "parent"
entity_type = "ParentEntity"
id_field = "parent_id"
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        assert_eq!(a.context_entities.len(), 1);
        assert_eq!(a.context_entities[0].name, "parent");
        assert_eq!(a.context_entities[0].entity_type, "ParentEntity");
        assert_eq!(a.context_entities[0].id_field, "parent_id");
    }

    #[test]
    fn state_var_parsing() {
        let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[state]]
name = "ready"
type = "bool"
initial = "false"
"#;
        let a: Automaton = toml::from_str(toml_src).unwrap();
        assert_eq!(a.state.len(), 2);
        assert_eq!(a.state[0].name, "count");
        assert_eq!(a.state[0].var_type, "counter");
        assert_eq!(a.state[1].var_type, "bool");
        assert_eq!(a.state[1].initial, "false");
    }
}
