//! Governed first-entity bootstrap contracts.

use serde::{Deserialize, Serialize};

use super::SchemaScopeV1;

/// Optional initial action executed after entity creation commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapActionV1 {
    /// Canonical action name from the pinned IOA closure.
    pub action: String,
    /// Canonical action parameter object.
    pub parameters: serde_json::Map<String, serde_json::Value>,
}

/// Idempotent request to enter one still-active scoped deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapDispatchRequestV1 {
    /// Correlation identity returned in the durable receipt.
    pub request_id: String,
    /// Caller-local idempotency key bound to the canonical request digest.
    pub idempotency_key: String,
    /// Original activation request identity retained by the active pointer.
    pub activation_request_id: String,
    /// Fully-qualified CSDL entity type in the exact pinned bundle.
    pub entity_type: String,
    /// Stable tenant-local entity identity.
    pub entity_id: String,
    /// Canonical initial entity field object.
    pub initial_fields: serde_json::Map<String, serde_json::Value>,
    /// Optional action dispatched after creation commits.
    pub initial_action: Option<BootstrapActionV1>,
}

/// Exact immutable deployment pin returned without accepting one from input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapSchemaPinV1 {
    /// Host-resolved task-local scope.
    pub scope: SchemaScopeV1,
    /// Host-resolved immutable canonical bundle digest.
    pub bundle_digest: String,
}

/// Closed stage at which a durable bootstrap outcome failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapFailureStageV1 {
    /// Exact-bundle validation failed before creation.
    Validation,
    /// Cedar denied the dedicated bridge action.
    Authorization,
    /// Scoped entity creation did not commit.
    Creation,
    /// Creation committed but the optional action did not.
    Action,
    /// Durable coordinator persistence did not complete normally.
    Persistence,
    /// A different operation owns the exact target.
    Conflict,
    /// A declared request or response budget was consumed.
    Budget,
}

/// Bounded structured failure retained in an authoritative receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapFailureV1 {
    /// Durable failure stage.
    pub stage: BootstrapFailureStageV1,
    /// Stable machine-readable code.
    pub code: String,
    /// Bounded human-readable summary.
    pub message: String,
    /// Whether retrying the same operation may make progress.
    pub retryable: bool,
    /// Pending Cedar decision identity, when authorization was denied.
    pub decision_id: Option<String>,
    /// Bounded structured diagnostic details.
    #[serde(default)]
    pub details: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Durable bootstrap-specific receipt replayed exactly after restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapDispatchReceiptV1 {
    /// Original accepted request identity.
    pub request_id: String,
    /// Exact immutable schema pin used for creation and dispatch.
    pub pin: BootstrapSchemaPinV1,
    /// Fully-qualified entity type.
    pub entity_type: String,
    /// Stable entity identity.
    pub entity_id: String,
    /// Authoritative creation sequence when creation committed.
    pub creation_sequence: Option<u64>,
    /// Authoritative optional action sequence when the action committed.
    pub action_sequence: Option<u64>,
    /// Typed action result encoded as canonical JSON when present.
    pub action_result: Option<serde_json::Value>,
    /// Bounded terminal failure, including honest post-creation failures.
    pub failure: Option<BootstrapFailureV1>,
}
