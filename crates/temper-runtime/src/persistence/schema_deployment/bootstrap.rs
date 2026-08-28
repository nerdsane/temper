//! Durable coordinator state for governed first-entity bootstrap dispatch.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::SchemaExecutionPin;

/// Optional initial action retained as canonical coordinator input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaBootstrapAction {
    /// Canonical action name.
    pub action: String,
    /// Canonical JSON parameter object.
    pub canonical_parameters_json: String,
    /// Durable action idempotency identity derived from the reservation.
    pub idempotency_key: String,
}

/// Immutable command atomically reserved against one active pointer and target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReserveSchemaBootstrap {
    /// Host-resolved tenant.
    pub tenant: String,
    /// Stable digest of host-resolved caller authority.
    pub caller_authority: String,
    /// Exact accepted security context used for Cedar and recovery.
    pub accepted_authority_json: String,
    /// Caller-local idempotency key.
    pub idempotency_key: String,
    /// Digest of every canonical request field and caller authority.
    pub request_digest: String,
    /// Original request identity returned on replay.
    pub request_id: String,
    /// Original activation request identity selecting durable host state.
    pub activation_request_id: String,
    /// Fully-qualified entity type.
    pub entity_type: String,
    /// Stable tenant-local entity identity.
    pub entity_id: String,
    /// Canonical JSON initial field object.
    pub canonical_initial_fields_json: String,
    /// Optional initial action.
    pub initial_action: Option<SchemaBootstrapAction>,
}

/// Durable coordinator phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaBootstrapStatus {
    /// Active pointer and target ownership were reserved.
    Reserved,
    /// Entity creation committed and its sequence was recorded.
    Created,
    /// Authoritative receipt was persisted.
    Completed,
}

/// Closed durable failure stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaBootstrapFailureStage {
    /// Exact-bundle request validation failed.
    Validation,
    /// Cedar denied the dedicated bridge action.
    Authorization,
    /// Entity creation did not commit.
    Creation,
    /// Optional action failed after creation.
    Action,
    /// Coordinator persistence did not complete normally.
    Persistence,
    /// Another operation owns the target.
    Conflict,
    /// A declared budget was consumed.
    Budget,
}

/// Bounded failure retained in the exact replay receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaBootstrapFailure {
    /// Failure stage.
    pub stage: SchemaBootstrapFailureStage,
    /// Stable machine-readable code.
    pub code: String,
    /// Bounded human-readable summary.
    pub message: String,
    /// Whether the same reservation may make progress when retried.
    pub retryable: bool,
    /// Pending Cedar decision identity when present.
    pub decision_id: Option<String>,
    /// Bounded ordered details.
    pub details: BTreeMap<String, serde_json::Value>,
}

/// Exact authoritative result persisted by the coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaBootstrapReceipt {
    /// Original accepted request identity.
    pub request_id: String,
    /// Exact immutable schema pin used by the scoped actor.
    pub pin: SchemaExecutionPin,
    /// Fully-qualified entity type.
    pub entity_type: String,
    /// Stable entity identity.
    pub entity_id: String,
    /// Authoritative creation sequence when committed.
    pub creation_sequence: Option<u64>,
    /// Authoritative action sequence when committed.
    pub action_sequence: Option<u64>,
    /// Canonical JSON action result when present.
    pub canonical_action_result_json: Option<String>,
    /// Bounded terminal failure when present.
    pub failure: Option<SchemaBootstrapFailure>,
}

/// Durable bootstrap reservation, progress, and final receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaBootstrapOperation {
    /// Immutable accepted command.
    pub command: ReserveSchemaBootstrap,
    /// Exact pin resolved atomically from the active pointer.
    pub pin: SchemaExecutionPin,
    /// Monotonic coordinator phase.
    pub status: SchemaBootstrapStatus,
    /// Authoritative committed creation sequence.
    pub creation_sequence: Option<u64>,
    /// Durable initial-action rejection retained before receipt finalization.
    #[serde(default)]
    pub action_failure: Option<SchemaBootstrapFailure>,
    /// Final exact receipt when completed.
    pub receipt: Option<SchemaBootstrapReceipt>,
    /// Monotonic compare-and-set sequence.
    pub committed_sequence: u64,
}

/// Result of reserving or replaying one operation key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReserveSchemaBootstrapOutcome {
    /// A new operation and target ownership claim committed.
    Reserved(SchemaBootstrapOperation),
    /// The exact prior operation was returned without another write.
    Replayed(SchemaBootstrapOperation),
}

/// Compare-and-set creation progress update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSchemaBootstrapCreated {
    /// Host-resolved tenant.
    pub tenant: String,
    /// Stable caller authority digest.
    pub caller_authority: String,
    /// Caller-local idempotency key.
    pub idempotency_key: String,
    /// Expected coordinator sequence.
    pub expected_sequence: u64,
    /// Authoritative entity creation sequence.
    pub creation_sequence: u64,
}

/// Compare-and-set durable initial-action rejection update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSchemaBootstrapActionFailure {
    /// Host-resolved tenant.
    pub tenant: String,
    /// Stable caller authority digest.
    pub caller_authority: String,
    /// Caller-local idempotency key.
    pub idempotency_key: String,
    /// Expected coordinator sequence.
    pub expected_sequence: u64,
    /// Exact bounded rejection outcome.
    pub failure: SchemaBootstrapFailure,
}

/// Compare-and-set terminal receipt update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteSchemaBootstrap {
    /// Host-resolved tenant.
    pub tenant: String,
    /// Stable caller authority digest.
    pub caller_authority: String,
    /// Caller-local idempotency key.
    pub idempotency_key: String,
    /// Expected coordinator sequence.
    pub expected_sequence: u64,
    /// Exact authoritative receipt.
    pub receipt: SchemaBootstrapReceipt,
}

/// Validate bounded coordinator input before any durable store accepts it.
pub fn validate_schema_bootstrap_reservation(
    command: &ReserveSchemaBootstrap,
) -> Result<(), String> {
    if let Some(action) = &command.initial_action {
        validate_bootstrap_text("initial action", &action.action, 256)?;
        validate_bootstrap_text("action idempotency key", &action.idempotency_key, 384)?;
        if action.canonical_parameters_json.len() > 1_048_576 {
            return Err("action parameters exceed the 1048576-byte budget".into());
        }
        let value: serde_json::Value = serde_json::from_str(&action.canonical_parameters_json)
            .map_err(|error| format!("action parameters are not valid JSON: {error}"))?;
        if !value.is_object() {
            return Err("action parameters must be a JSON object".into());
        }
    }
    Ok(())
}

/// Validate bounded authoritative receipt content before persistence.
pub fn validate_schema_bootstrap_receipt(receipt: &SchemaBootstrapReceipt) -> Result<(), String> {
    validate_bootstrap_text("receipt request id", &receipt.request_id, 256)?;
    validate_bootstrap_text("receipt entity type", &receipt.entity_type, 256)?;
    validate_bootstrap_text("receipt entity id", &receipt.entity_id, 256)?;
    if let Some(result) = &receipt.canonical_action_result_json {
        if result.len() > 1_048_576 {
            return Err("action result exceeds the 1048576-byte budget".into());
        }
        let value: serde_json::Value = serde_json::from_str(result)
            .map_err(|error| format!("action result is not valid JSON: {error}"))?;
        validate_bounded_json(&value, 0)?;
    }
    if let Some(failure) = &receipt.failure {
        validate_schema_bootstrap_failure(failure)?;
    }
    Ok(())
}

/// Validate one bounded durable bootstrap failure.
pub fn validate_schema_bootstrap_failure(failure: &SchemaBootstrapFailure) -> Result<(), String> {
    validate_bootstrap_text("failure code", &failure.code, 128)?;
    validate_bootstrap_text("failure message", &failure.message, 1_024)?;
    if let Some(decision_id) = &failure.decision_id {
        validate_bootstrap_text("failure decision id", decision_id, 256)?;
    }
    if failure.details.len() > 64 {
        return Err("failure details exceed the 64-item budget".into());
    }
    for (key, value) in &failure.details {
        validate_bootstrap_text("failure detail key", key, 128)?;
        validate_bounded_json(value, 0)?;
    }
    let encoded = serde_json::to_vec(failure)
        .map_err(|error| format!("failure is not serializable: {error}"))?;
    if encoded.len() > 262_144 {
        return Err("failure exceeds the 262144-byte budget".into());
    }
    Ok(())
}

fn validate_bootstrap_text(name: &str, value: &str, budget: usize) -> Result<(), String> {
    if value.trim().is_empty() || value.trim() != value || value.len() > budget {
        return Err(format!(
            "{name} must be non-empty, canonical, and at most {budget} bytes"
        ));
    }
    Ok(())
}

fn validate_bounded_json(value: &serde_json::Value, depth: usize) -> Result<(), String> {
    if depth > 8 {
        return Err("structured value exceeds the depth budget".into());
    }
    match value {
        serde_json::Value::String(value) if value.len() > 1_024 => {
            Err("structured string exceeds the 1024-byte budget".into())
        }
        serde_json::Value::Array(values) if values.len() > 64 => {
            Err("structured array exceeds the 64-item budget".into())
        }
        serde_json::Value::Object(values) if values.len() > 64 => {
            Err("structured object exceeds the 64-item budget".into())
        }
        serde_json::Value::Array(values) => values
            .iter()
            .try_for_each(|value| validate_bounded_json(value, depth + 1)),
        serde_json::Value::Object(values) => values.iter().try_for_each(|(key, value)| {
            validate_bootstrap_text("structured key", key, 128)?;
            validate_bounded_json(value, depth + 1)
        }),
        _ => Ok(()),
    }
}
