//! Actions and the metadata declared on them.

use serde::{Deserialize, Serialize};

use super::state::ActionParam;
use super::trigger::ActionTrigger;
pub use crate::predicate::Effect;
use crate::predicate::Expr;

/// How an action relates to the environment (I/O automata).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    /// Arrives from the environment (an HTTP request); always enabled when it
    /// has no `from`.
    Input,
    /// A private state transition.
    #[default]
    Internal,
    /// Emitted to the environment; never transitions the entity.
    Output,
    /// One governed intent that may produce declared sub-writes (ADR-0040);
    /// enabled like an input action.
    Composite,
}

impl std::fmt::Display for ActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ActionKind {
    /// Spec spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::Input => "input",
            ActionKind::Internal => "internal",
            ActionKind::Output => "output",
            ActionKind::Composite => "composite",
        }
    }

    /// Whether an action of this kind with no `from` fires from every state.
    pub fn enabled_everywhere(self) -> bool {
        matches!(self, ActionKind::Input | ActionKind::Composite)
    }
}

/// An action in the I/O Automaton.
///
/// Each action has a precondition (`from` and `guard`) and effects (state
/// changes); its `kind` says how it relates to the environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    /// Action name (e.g., "SubmitOrder").
    pub name: String,
    /// How the action relates to the environment.
    #[serde(default)]
    pub kind: ActionKind,
    /// Precondition: states from which this action can fire.
    #[serde(default)]
    pub from: Vec<String>,
    /// Effect: the target state after this action fires.
    pub to: Option<String>,
    /// Precondition over the pre-state, beyond `from`. `true` when absent.
    #[serde(default = "Expr::always", skip_serializing_if = "Expr::is_always")]
    pub guard: Expr,
    /// Effects beyond state change.
    #[serde(default)]
    pub effect: Vec<Effect>,
    /// Parameters this action accepts.
    #[serde(default)]
    pub params: Vec<ActionParam>,
    /// Atomic checks of incoming parameters against the pre-transition state.
    #[serde(default)]
    pub constraints: Vec<ActionConstraint>,
    /// Agent hint for this action.
    pub hint: Option<String>,
    /// What the action does in plain words, for this block's arrow on a
    /// statechart; non-empty when present. A label only: nothing runs or
    /// verifies on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub says: Option<String>,
    /// Whether a composite action records an audit/idempotency event on the
    /// parent stream. Only written on `kind = "composite"`.
    #[serde(default = "default_record_parent_event")]
    pub record_parent_event: bool,
    /// Outgoing triggers fired post-commit of this action (ADR-0046).
    ///
    /// Each trigger describes one cross-entity dispatch, WASM module
    /// invocation, adapter, webhook or hook call. Triggers fire after the
    /// source action's transition commits (fire-and-forget — failures do not
    /// roll back the source). Kind-specific fields are validated at parse time.
    #[serde(default, rename = "triggers")]
    pub triggers: Vec<ActionTrigger>,
    /// Composite-action Cedar gate declaration (ADR-0040), written as one
    /// `[[action.cedar_gate]]`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "one_cedar_gate"
    )]
    pub cedar_gate: Option<CompositeCedarGate>,
    /// Declared sub-write contract for composite actions (ADR-0040).
    #[serde(default, rename = "sub_writes")]
    pub sub_writes: Vec<SubWriteSpec>,
}

/// `[[action.cedar_gate]]` is an array of tables holding exactly one gate; a
/// single inline table is accepted too.
fn one_cedar_gate<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<CompositeCedarGate>, D::Error> {
    use serde::de::Error;
    let gate = match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Array(mut gates) if gates.len() == 1 => gates.remove(0),
        serde_json::Value::Array(gates) => {
            return Err(D::Error::custom(format!(
                "cedar_gate: a composite action has one Cedar gate, found {}",
                gates.len()
            )));
        }
        table => table,
    };
    serde_json::from_value(gate)
        .map(Some)
        .map_err(|e| D::Error::custom(format!("cedar_gate: {e}")))
}

/// A parameter constraint evaluated before any action effect or field write.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionConstraint {
    /// Require the parameter to equal the stored field.
    ParamEqualsField { param: String, field: String },
    /// Require the parameter to differ from the stored field.
    ParamNotEqualsField { param: String, field: String },
    /// Require an integer parameter to exceed the stored integer field.
    ParamGreaterThanField { param: String, field: String },
    /// Require a nonempty string parameter.
    ParamNonempty { param: String },
}

impl ActionConstraint {
    /// Parameter checked by this constraint.
    pub fn param(&self) -> &str {
        match self {
            Self::ParamEqualsField { param, .. }
            | Self::ParamNotEqualsField { param, .. }
            | Self::ParamGreaterThanField { param, .. }
            | Self::ParamNonempty { param } => param,
        }
    }

    /// Pre-state field used by a comparison constraint.
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::ParamEqualsField { field, .. }
            | Self::ParamNotEqualsField { field, .. }
            | Self::ParamGreaterThanField { field, .. } => Some(field),
            Self::ParamNonempty { .. } => None,
        }
    }
}

fn default_record_parent_event() -> bool {
    true
}

/// The single Cedar gate evaluated for a composite action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompositeCedarGate {
    /// Principal expression, usually `request.principal`.
    pub principal: String,
    /// Resource expression, usually `this`.
    pub resource: String,
    /// Cedar action identifier for the composite intent.
    pub action: String,
}

/// Declared write shape emitted by a composite action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SubWriteSpec {
    /// Target entity type receiving the write.
    pub target_entity: String,
    /// Target action, e.g. `Create` or `Update`.
    pub action: String,
    /// Handler input or source that generates this write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_from: Option<String>,
}
