//! Versioned collection-workflow intents and lifecycle state.

use serde::{Deserialize, Serialize};
use temper_runtime::persistence::schema_deployment::SchemaEventPin;

use crate::trigger::delivery::ReactionDeliveryStatus;

/// Version understood by the additive collection ledger reader.
pub(crate) const COLLECTION_LEDGER_VERSION: u16 = 1;
/// ADR-0181 maximum sealed roster size.
pub(crate) const MAX_COLLECTION_MEMBERS: u16 = 64;
/// ADR-0181 maximum concurrent member deliveries.
pub(crate) const MAX_COLLECTION_CONCURRENCY: u8 = 8;
/// ADR-0181 maximum automatic attempts per member.
pub(crate) const MAX_COLLECTION_ATTEMPTS: u8 = 5;

/// Static budgets copied into each workflow journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CollectionWorkflowBudgets {
    pub(crate) max_members: u16,
    pub(crate) max_concurrency: u8,
    pub(crate) max_attempts: u8,
}

impl CollectionWorkflowBudgets {
    /// Validate the closed v1 budget range.
    pub(crate) fn validate(self) -> Result<Self, String> {
        if self.max_members == 0 || self.max_members > MAX_COLLECTION_MEMBERS {
            return Err("max_members must be in 1..=64".to_string());
        }
        if self.max_concurrency == 0
            || self.max_concurrency > MAX_COLLECTION_CONCURRENCY
            || u16::from(self.max_concurrency) > self.max_members
        {
            return Err("max_concurrency must be in 1..=8 and not exceed max_members".to_string());
        }
        if self.max_attempts == 0 || self.max_attempts > MAX_COLLECTION_ATTEMPTS {
            return Err("max_attempts must be in 1..=5".to_string());
        }
        Ok(self)
    }
}

/// Immutable input needed to create one start intent and lifecycle journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CollectionWorkflowStart {
    pub(crate) tenant: String,
    pub(crate) source_entity_type: String,
    pub(crate) source_entity_id: String,
    pub(crate) declaration_name: String,
    pub(crate) source_action: String,
    pub(crate) source_sequence: u64,
    pub(crate) schema_digest: String,
    pub(crate) schema_pin: Option<SchemaEventPin>,
    pub(crate) authority: serde_json::Value,
    pub(crate) roster: Vec<String>,
    pub(crate) budgets: CollectionWorkflowBudgets,
}

/// Normalized start evidence co-committed with the source event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CollectionStartIntentV1 {
    pub(crate) version: u16,
    pub(crate) workflow_id: String,
    pub(crate) start: CollectionWorkflowStart,
}

/// Requested terminal outcome for the first committed control action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CollectionRequestedOutcome {
    Cancelled,
    TimedOut,
}

impl CollectionRequestedOutcome {
    pub(crate) fn identity_component(self) -> &'static str {
        match self {
            Self::Cancelled => "Cancelled",
            Self::TimedOut => "TimedOut",
        }
    }
}

/// Normalized control evidence co-committed with the source event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CollectionControlIntentV1 {
    pub(crate) version: u16,
    pub(crate) control_id: String,
    pub(crate) workflow_id: String,
    pub(crate) requested_outcome: CollectionRequestedOutcome,
    pub(crate) source_action: String,
    pub(crate) source_sequence: u64,
    pub(crate) control_epoch: u64,
    pub(crate) authority: serde_json::Value,
    pub(crate) schema_pin: Option<SchemaEventPin>,
}

/// Durable workflow lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CollectionWorkflowStatus {
    Running,
    Cancelling,
    TimingOut,
    Succeeded,
    PartiallyFailed,
    Failed,
    Cancelled,
    TimedOut,
}

impl CollectionWorkflowStatus {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::PartiallyFailed
                | Self::Failed
                | Self::Cancelled
                | Self::TimedOut
        )
    }
}

/// Durable member lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CollectionMemberStatus {
    Pending,
    InFlight,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

impl CollectionMemberStatus {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }
}

/// Sanitized closed failure evidence retained for one member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CollectionFailureClass {
    IdentityCollision,
    PermanentRejected,
    AttemptsExhausted,
    DeliverySkipped,
    UnsupportedDropAllowed,
    CancellationFailed,
}

/// Exact target receipt evidence used to reconcile duplicate completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CollectionMemberReceipt {
    pub(crate) delivery_id: String,
    pub(crate) fencing_token: u64,
}

/// One member in the immutable sealed roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CollectionMemberRecord {
    pub(crate) member_id: String,
    pub(crate) child_entity_id: String,
    pub(crate) member_index: u32,
    pub(crate) member_value: String,
    pub(crate) status: CollectionMemberStatus,
    pub(crate) admission_control_epoch: Option<u64>,
    pub(crate) terminal_control_epoch: Option<u64>,
    pub(crate) attempts: u8,
    pub(crate) delivery_id: Option<String>,
    pub(crate) delivery_status: Option<ReactionDeliveryStatus>,
    pub(crate) receipt: Option<CollectionMemberReceipt>,
    pub(crate) failure_class: Option<CollectionFailureClass>,
}

/// Persisted aggregate counts, checked against the bounded member roster.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CollectionWorkflowCounts {
    pub(crate) pending: u16,
    pub(crate) in_flight: u16,
    pub(crate) succeeded: u16,
    pub(crate) failed: u16,
    pub(crate) cancelled: u16,
    pub(crate) timed_out: u16,
}

impl CollectionWorkflowCounts {
    pub(crate) fn total(self) -> u16 {
        self.pending
            + self.in_flight
            + self.succeeded
            + self.failed
            + self.cancelled
            + self.timed_out
    }

    pub(crate) fn terminal(self) -> u16 {
        self.succeeded + self.failed + self.cancelled + self.timed_out
    }
}

/// State of the eventual join delivery; execution is added by later issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CollectionJoinStatus {
    Pending,
    InFlight,
    Delivered,
    DeliveryFailed,
}

/// Complete replayable v1 workflow ledger snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CollectionWorkflowRecordV1 {
    pub(crate) version: u16,
    pub(crate) workflow_id: String,
    pub(crate) tenant: String,
    pub(crate) source_entity_type: String,
    pub(crate) source_entity_id: String,
    pub(crate) declaration_name: String,
    pub(crate) source_action: String,
    pub(crate) source_sequence: u64,
    pub(crate) schema_digest: String,
    pub(crate) schema_pin: Option<SchemaEventPin>,
    pub(crate) original_authority: serde_json::Value,
    pub(crate) sealed_roster: Vec<String>,
    pub(crate) budgets: CollectionWorkflowBudgets,
    pub(crate) next_undispatched_index: u16,
    pub(crate) control_epoch: u64,
    pub(crate) status: CollectionWorkflowStatus,
    pub(crate) requested_outcome: Option<CollectionRequestedOutcome>,
    pub(crate) terminal_classification: Option<CollectionWorkflowStatus>,
    pub(crate) join_status: CollectionJoinStatus,
    pub(crate) counts: CollectionWorkflowCounts,
    pub(crate) total_attempts: u32,
    pub(crate) members: Vec<CollectionMemberRecord>,
    pub(crate) last_control_id: Option<String>,
    pub(crate) control_source_action: Option<String>,
    pub(crate) control_source_sequence: Option<u64>,
    pub(crate) control_authority: Option<serde_json::Value>,
    pub(crate) control_schema_pin: Option<SchemaEventPin>,
}

/// Result of an idempotent lifecycle mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionMutationOutcome {
    Applied,
    Replayed,
    IgnoredAfterFirstControl,
}

/// Terminal member evidence supplied by the durable delivery owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollectionMemberTerminalEvidence {
    pub(crate) member_id: String,
    pub(crate) control_epoch: u64,
    pub(crate) status: CollectionMemberStatus,
    pub(crate) attempts: u8,
    pub(crate) delivery_id: Option<String>,
    pub(crate) delivery_status: ReactionDeliveryStatus,
    pub(crate) receipt: Option<CollectionMemberReceipt>,
    pub(crate) failure_class: Option<CollectionFailureClass>,
}
