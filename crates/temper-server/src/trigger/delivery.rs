//! Durable reaction delivery identities and lifecycle records (ADR-0158).
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use temper_runtime::persistence::PersistenceError;
use temper_runtime::persistence::schema_deployment::SchemaEventPin;

use super::types::ReactionRule;

mod awaited;
mod failure;
mod identity;
mod payload;
mod persistence;
mod state_timeout;
pub(crate) use failure::{DurableFailureKind, delivery_failure_envelope};
pub use identity::stable_delivery_id;
pub use payload::{attach_intents, attach_receipt, extract_intents, extract_receipt};
pub(crate) use persistence::delivery_record_append;
pub use persistence::{
    append_delivery_record, delivery_journal_id, find_delivery_record, initialize_delivery_record,
    list_delivery_records, list_delivery_records_page, load_delivery_record,
};
pub(crate) use state_timeout::state_timeout_declaration_id;
pub use state_timeout::{
    DeliveryKind, STATE_TIMEOUT_CLOCK_AUDIT_BUDGET, STATE_TIMEOUT_SERVICE,
    StateTimeoutIntentContext, StateTimeoutPrecondition, state_timeout_intents,
    transition_table_digest,
};

/// Reserved event-payload field holding intents co-committed with a source event.
pub const REACTION_INTENTS_FIELD: &str = "_temper_reaction_intents_v1";
/// Reserved target-event field proving one fenced delivery reached commit.
pub const REACTION_RECEIPT_FIELD: &str = "_temper_reaction_receipt_v1";
/// Reserved event parameter carrying durable timeout occurrence evidence.
pub const STATE_TIMEOUT_OCCURRENCE_FIELD: &str = "_temper_state_timeout_declaration_v1";
/// Maximum automatic delivery attempts before transient failure dead-letters.
pub const MAX_AUTOMATIC_ATTEMPTS: u32 = 5;
/// Maximum operator-requested retries for one transient dead letter.
pub const MAX_MANUAL_RETRIES: u32 = 3;
/// Private synthetic entity type used for one journal per logical delivery.
pub const REACTION_DELIVERY_ENTITY_TYPE: &str = "_ReactionDelivery";
/// Maximum private callback evidence retained for one awaited integration.
pub const MAX_AWAITED_CALLBACK_EVIDENCE_BYTES: usize = 128 * 1024;
/// Bounded rule and authority snapshot supplied to the entity actor at commit.
#[derive(Debug, Clone)]
pub struct ReactionCommitContext {
    /// Candidate rules selected from the current tenant registry version.
    pub rules: Vec<ReactionRule>,
    /// Original Cedar authority serialized for private persistence.
    pub authority: serde_json::Value,
    /// Descendant depth consumed by intents created by this action.
    pub depth: u32,
    /// Existing root delivery for cascades; absent for top-level source actions.
    pub root_delivery_id: Option<String>,
    /// Source sequence used when resolving cross-entity guard inputs.
    pub expected_source_sequence: u64,
    /// Cross-entity guard inputs sampled before the source transition, keyed
    /// by stable rule name. The actor combines these with its exact committed
    /// post-state so restart timing cannot change the guard decision.
    pub resolved_guards: std::collections::BTreeMap<String, crate::trigger::guard::CrossStatusMap>,
    /// Receipt to co-commit when this action is a reaction target.
    pub receipt: Option<ReactionReceipt>,
}

/// Receipt co-committed with a target event for reconciliation after crashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionReceipt {
    /// Stable logical delivery identity.
    pub delivery_id: String,
    /// Lease fence that authorized this target attempt.
    pub fencing_token: u64,
    /// Target commit time from the scheduler clock.
    pub received_at: DateTime<Utc>,
    /// State whose successful timeout firing this receipt proves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_timeout_state: Option<String>,
    /// Exact target action schema, absent only for tenant-global compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_pin: Option<SchemaEventPin>,
    /// Collection workflow fence checked at the target commit boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<crate::trigger::collection_workflow::CollectionDeliveryContext>,
    /// Awaited callback whose acceptance must be co-committed with the target event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) awaited_callback: Option<AwaitedCallbackReceiptV1>,
}

/// Exact awaited execution authorized to commit one callback target event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AwaitedCallbackReceiptV1 {
    pub(crate) execution_id: String,
    pub(crate) callback_action: String,
}

/// Immutable normalized reaction input committed with the source event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedReactionIntent {
    /// Kernel delivery category.
    #[serde(default)]
    pub kind: DeliveryKind,
    /// Stable logical delivery identity.
    pub delivery_id: String,
    /// Root delivery identity for descendant-tree waits.
    pub root_delivery_id: String,
    /// Owning tenant.
    pub tenant: String,
    /// Source entity type.
    pub source_entity_type: String,
    /// Source entity identifier.
    pub source_entity_id: String,
    /// Committed source action.
    pub source_action: String,
    /// Committed source journal sequence.
    pub source_sequence: u64,
    /// Source state after the action.
    pub source_to_state: String,
    /// Exact post-transition source fields used for resolution and guards.
    pub source_fields: serde_json::Value,
    /// Kernel-attested source stream descriptor, when the source commit
    /// published stream content. Targets must not infer this authority from
    /// user-controlled action fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_stream_descriptor: Option<temper_runtime::persistence::StreamDescriptorV1>,
    /// Guard decision made from the committed source post-state and the
    /// pre-transition cross-entity snapshot.
    pub guard_passed: bool,
    /// Target identifier resolved once at source commit.
    pub target_entity_id: Option<String>,
    /// Stable trigger name.
    pub trigger_name: String,
    /// Stable trigger index within the action candidate set.
    pub trigger_index: usize,
    /// Cascade depth consumed by this delivery.
    pub depth: u32,
    /// Serialized registry rule bound at source commit.
    pub rule: serde_json::Value,
    /// Serialized original Cedar authority; never returned unredacted by Observe.
    pub authority: serde_json::Value,
    /// Logical creation time from the scheduler clock.
    pub created_at: DateTime<Utc>,
    /// Earliest absolute scheduler time at which this delivery may be claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<Utc>>,
    /// State-clock evidence for generated timeout deliveries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_timeout: Option<StateTimeoutPrecondition>,
    /// Collection execution fence for member, cancellation, or join delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<crate::trigger::collection_workflow::CollectionDeliveryContext>,
    /// Exact source action schema, absent only for tenant-global compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_pin: Option<SchemaEventPin>,
}

/// Durable delivery lifecycle from persisted intent through terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionDeliveryStatus {
    /// Eligible for a worker claim.
    Pending,
    /// Leased by one fenced worker.
    Claimed,
    /// Target action is in flight or awaiting receipt reconciliation.
    Dispatching,
    /// Target event and receipt are durable.
    Succeeded,
    /// The committed candidate did not match its post-state or guard.
    Skipped,
    /// A failure explicitly permitted by `drop_ok`.
    DroppedAllowed,
    /// Permanent Cedar or validation rejection.
    Rejected,
    /// Bounded transient attempts were exhausted.
    DeadLettered,
}

/// Immutable identity of the single awaited WASM integration bound to a
/// collection-member delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AwaitedExecutionIdentityV1 {
    pub(crate) execution_id: String,
    pub(crate) integration_name: String,
    pub(crate) module_name: String,
    pub(crate) module_digest: String,
    pub(crate) success_callback: String,
    pub(crate) failure_callback: Option<String>,
    pub(crate) schema_pin: Option<SchemaEventPin>,
    pub(crate) deadline: DateTime<Utc>,
}

/// Durable boundary reached by one awaited collection-member integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AwaitedExecutionPhase {
    Executing,
    ExecutionSucceeded,
    ExecutionFailed,
    CallbackAccepted,
}

/// Closed failure outcomes retained without relying on diagnostic strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AwaitedExecutionFailureClass {
    ModuleFailure,
    CallbackRejected,
    CallbackTimeout,
    CallbackStorageFailure,
}

/// Typed ownership failure evidence independent of execution completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AwaitedOwnerFailureClass {
    RenewalLost,
    StorageFailure,
    DeadlineElapsed,
}

/// Last fenced owner failure retained for recovery and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AwaitedOwnerFailureEvidenceV1 {
    pub(crate) class: AwaitedOwnerFailureClass,
    pub(crate) fencing_token: u64,
    pub(crate) occurred_at: DateTime<Utc>,
}

/// Private replay evidence for an awaited integration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AwaitedExecutionEvidenceV1 {
    pub(crate) identity: AwaitedExecutionIdentityV1,
    pub(crate) phase: AwaitedExecutionPhase,
    pub(crate) fencing_token: u64,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) callback_action: Option<String>,
    pub(crate) callback_params: Option<serde_json::Value>,
    pub(crate) callback_digest: Option<String>,
    pub(crate) callback_accepted_at: Option<DateTime<Utc>>,
    pub(crate) callback_sequence: Option<u64>,
    pub(crate) execution_failure: Option<AwaitedExecutionFailureClass>,
    pub(crate) callback_failure: Option<AwaitedExecutionFailureClass>,
}

impl ReactionDeliveryStatus {
    /// Whether normal automatic delivery work has ended.
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Skipped
                | Self::DroppedAllowed
                | Self::Rejected
                | Self::DeadLettered
        )
    }
}

/// Mutable durable state for one logical delivery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReactionDeliveryRecord {
    /// Immutable intent.
    pub intent: PersistedReactionIntent,
    /// Current lifecycle state.
    pub status: ReactionDeliveryStatus,
    /// Automatic attempts consumed.
    pub attempts: u32,
    /// Manual retry requests consumed.
    pub manual_retries: u32,
    /// Monotonic lease fence.
    pub fencing_token: u64,
    /// Lease expiry under the scheduler clock.
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// Earliest scheduler time at which another automatic claim is allowed.
    #[serde(default)]
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Whether the last terminal failure was classified transient.
    pub transient_failure: bool,
    /// Sanitized last failure reason.
    pub last_error: Option<String>,
    /// Exact private execution evidence for an awaited collection member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) awaited_execution: Option<AwaitedExecutionEvidenceV1>,
    /// Typed failure from the most recent awaited execution owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) awaited_owner_failure: Option<AwaitedOwnerFailureEvidenceV1>,
    /// Canonical typed failure for the latest terminal unsuccessful outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<temper_failure::FailureEnvelopeV1>,
}

impl ReactionDeliveryRecord {
    /// Create the initial pending record for a committed intent.
    pub fn pending(intent: PersistedReactionIntent) -> Self {
        let next_attempt_at = intent.not_before;
        Self {
            intent,
            status: ReactionDeliveryStatus::Pending,
            attempts: 0,
            manual_retries: 0,
            fencing_token: 0,
            lease_expires_at: None,
            next_attempt_at,
            transient_failure: false,
            last_error: None,
            awaited_execution: None,
            awaited_owner_failure: None,
            failure: None,
        }
    }

    /// Claim one pending delivery and return its new fencing token.
    pub fn claim(&mut self, now: DateTime<Utc>, lease: Duration) -> Result<u64, String> {
        if self.status != ReactionDeliveryStatus::Pending {
            return Err("delivery is not pending".to_string());
        }
        if lease <= Duration::zero() {
            return Err("delivery lease must be positive".to_string());
        }
        if self.next_attempt_at.is_some_and(|next| next > now) {
            return Err("delivery backoff has not elapsed".to_string());
        }
        if self.attempts >= MAX_AUTOMATIC_ATTEMPTS {
            return Err("automatic delivery attempt budget exhausted".to_string());
        }
        self.attempts += 1;
        self.fencing_token = self.fencing_token.saturating_add(1);
        self.lease_expires_at = Some(now + lease);
        self.next_attempt_at = None;
        self.status = ReactionDeliveryStatus::Claimed;
        Ok(self.fencing_token)
    }

    /// Return an expired claim to the pending pool without resetting budgets.
    pub fn recover_expired_lease(&mut self, now: DateTime<Utc>) -> bool {
        let recoverable = matches!(
            self.status,
            ReactionDeliveryStatus::Claimed | ReactionDeliveryStatus::Dispatching
        ) && self.lease_expires_at.is_some_and(|expiry| expiry <= now);
        if recoverable {
            self.status = ReactionDeliveryStatus::Pending;
            self.lease_expires_at = None;
        }
        recoverable
    }

    /// Fence the transition from claimed to target dispatching.
    pub fn begin_dispatch(&mut self, fencing_token: u64) -> Result<(), String> {
        self.require_fence(fencing_token, ReactionDeliveryStatus::Claimed)?;
        self.status = ReactionDeliveryStatus::Dispatching;
        Ok(())
    }

    /// Persist a bounded terminal transient failure.
    pub fn dead_letter(
        &mut self,
        fencing_token: u64,
        transient: bool,
        error: &str,
    ) -> Result<(), String> {
        self.require_fence(fencing_token, ReactionDeliveryStatus::Dispatching)?;
        self.status = ReactionDeliveryStatus::DeadLettered;
        self.lease_expires_at = None;
        self.transient_failure = transient;
        self.last_error = Some(error.to_string());
        self.failure = None;
        Ok(())
    }

    /// Request another attempt without replacing the original authority.
    pub fn request_manual_retry(&mut self) -> Result<u32, String> {
        if self.intent.collection.as_ref().is_some_and(|context| {
            context.role != crate::trigger::collection_workflow::CollectionDeliveryRole::Join
        }) {
            return Err("manual retry is forbidden for collection member lineages".to_string());
        }
        if self.status != ReactionDeliveryStatus::DeadLettered || !self.transient_failure {
            return Err("only transient dead letters can be retried".to_string());
        }
        if self.manual_retries >= MAX_MANUAL_RETRIES {
            return Err("manual retry budget exhausted".to_string());
        }
        self.manual_retries += 1;
        self.attempts = 0;
        self.status = ReactionDeliveryStatus::Pending;
        self.transient_failure = false;
        self.last_error = None;
        self.failure = None;
        self.next_attempt_at = None;
        Ok(self.manual_retries)
    }

    fn require_fence(
        &self,
        fencing_token: u64,
        required_status: ReactionDeliveryStatus,
    ) -> Result<(), String> {
        if self.status != required_status {
            return Err("delivery is in the wrong lifecycle state".to_string());
        }
        if self.fencing_token != fencing_token {
            return Err("stale delivery fencing token".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "delivery_test.rs"]
mod tests;
