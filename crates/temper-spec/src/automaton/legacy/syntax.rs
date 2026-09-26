//! The pre-grammar action-guard and trigger-guard shapes.

use serde::{Deserialize, Serialize};

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
    /// All child guards must pass.
    AllOf {
        /// Child guards.
        guards: Vec<TriggerGuard>,
    },
    /// At least one child guard must pass.
    AnyOf {
        /// Child guards.
        guards: Vec<TriggerGuard>,
    },
    /// Inverted child guard.
    Not {
        /// Inner guard.
        guard: Box<TriggerGuard>,
    },
}
