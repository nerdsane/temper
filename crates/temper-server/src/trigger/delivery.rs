//! Durable reaction delivery identities and lifecycle records (ADR-0158).
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use temper_runtime::persistence::schema_deployment::SchemaEventPin;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope, PersistenceError};
use temper_runtime::scheduler::{sim_now, sim_uuid};

use super::types::ReactionRule;
use crate::storage::BoxedEventStore;

mod identity;
mod state_timeout;
pub use identity::stable_delivery_id;
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
        Ok(())
    }

    /// Request another attempt without replacing the original authority.
    pub fn request_manual_retry(&mut self) -> Result<u32, String> {
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

/// Attach normalized intents to the source event payload before its single append.
pub fn attach_intents(
    payload: &mut serde_json::Value,
    intents: &[PersistedReactionIntent],
) -> Result<(), String> {
    if intents.is_empty() {
        return Ok(());
    }
    let object = payload
        .as_object_mut()
        .ok_or_else(|| "entity event payload must be an object".to_string())?;
    let value = serde_json::to_value(intents).map_err(|error| error.to_string())?;
    object.insert(REACTION_INTENTS_FIELD.to_string(), value);
    Ok(())
}

/// Read normalized intents from a replayed source event payload.
pub fn extract_intents(
    payload: &serde_json::Value,
) -> Result<Vec<PersistedReactionIntent>, String> {
    let Some(value) = payload.get(REACTION_INTENTS_FIELD) else {
        return Ok(Vec::new());
    };
    serde_json::from_value(value.clone()).map_err(|error| error.to_string())
}

/// Attach one delivery receipt to the target event before its append.
pub fn attach_receipt(
    payload: &mut serde_json::Value,
    receipt: &ReactionReceipt,
) -> Result<(), String> {
    let object = payload
        .as_object_mut()
        .ok_or_else(|| "entity event payload must be an object".to_string())?;
    let value = serde_json::to_value(receipt).map_err(|error| error.to_string())?;
    object.insert(REACTION_RECEIPT_FIELD.to_string(), value);
    Ok(())
}

/// Read a co-committed target receipt from a replayed event payload.
pub fn extract_receipt(payload: &serde_json::Value) -> Result<Option<ReactionReceipt>, String> {
    let Some(value) = payload.get(REACTION_RECEIPT_FIELD) else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Persistence ID of the private lifecycle journal for an intent.
pub fn delivery_journal_id(intent: &PersistedReactionIntent) -> String {
    format!(
        "{}:{REACTION_DELIVERY_ENTITY_TYPE}:{}",
        intent.tenant, intent.delivery_id
    )
}

/// Append one fenced lifecycle snapshot to the delivery's private journal.
pub async fn append_delivery_record(
    store: &BoxedEventStore,
    expected_sequence: u64,
    record: &ReactionDeliveryRecord,
) -> Result<u64, PersistenceError> {
    let payload = serde_json::to_value(record)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    let persistence_id = delivery_journal_id(&record.intent);
    let envelope = PersistenceEnvelope {
        sequence_nr: expected_sequence + 1,
        event_type: format!("ReactionDelivery::{:?}", record.status),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.clone(),
        },
    };
    store
        .append(&persistence_id, expected_sequence, &[envelope])
        .await
}

/// Restore the latest lifecycle snapshot, inferring `Pending` from the atomic
/// source intent when no lifecycle journal exists yet.
pub async fn load_delivery_record(
    store: &BoxedEventStore,
    intent: PersistedReactionIntent,
) -> Result<(ReactionDeliveryRecord, u64), PersistenceError> {
    let persistence_id = delivery_journal_id(&intent);
    let events = store.read_events(&persistence_id, 0).await?;
    let Some(latest) = events.last() else {
        return Ok((ReactionDeliveryRecord::pending(intent), 0));
    };
    let record: ReactionDeliveryRecord = serde_json::from_value(latest.payload.clone())
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    if record.intent.delivery_id != intent.delivery_id || record.intent.tenant != intent.tenant {
        return Err(PersistenceError::Serialization(
            "delivery journal identity does not match source intent".to_string(),
        ));
    }
    Ok((record, latest.sequence_nr))
}

/// Materialize the inferred Pending state so it is immediately queryable.
pub async fn initialize_delivery_record(
    store: &BoxedEventStore,
    intent: PersistedReactionIntent,
) -> Result<(), PersistenceError> {
    let (record, sequence) = load_delivery_record(store, intent).await?;
    if sequence != 0 {
        return Ok(());
    }
    match append_delivery_record(store, 0, &record).await {
        Ok(_) | Err(PersistenceError::ConcurrencyViolation { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}

/// List bounded delivery records inferred from committed source intents.
pub async fn list_delivery_records(
    store: &BoxedEventStore,
    tenant: &str,
    limit: usize,
) -> Result<Vec<(ReactionDeliveryRecord, u64)>, PersistenceError> {
    list_delivery_records_page(store, tenant, None, limit).await
}

/// Read one keyset page of current delivery lifecycle records.
pub async fn list_delivery_records_page(
    store: &BoxedEventStore,
    tenant: &str,
    after_delivery_id: Option<&str>,
    limit: usize,
) -> Result<Vec<(ReactionDeliveryRecord, u64)>, PersistenceError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    let mut after = after_delivery_id.map(|delivery_id| {
        (
            REACTION_DELIVERY_ENTITY_TYPE.to_string(),
            delivery_id.to_string(),
        )
    });
    while records.len() < limit {
        let page = store
            .list_journal_ids_page(
                tenant,
                Some(REACTION_DELIVERY_ENTITY_TYPE),
                after
                    .as_ref()
                    .map(|(entity_type, entity_id)| (entity_type.as_str(), entity_id.as_str())),
                limit.saturating_sub(records.len()).max(1),
            )
            .await?;
        if page.is_empty() {
            break;
        }
        after = page.last().cloned();
        for (entity_type, entity_id) in page {
            if entity_type != REACTION_DELIVERY_ENTITY_TYPE {
                continue;
            }
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let events = store.read_latest_events(&persistence_id, 1).await?;
            if let Some(latest) = events.last() {
                let record: ReactionDeliveryRecord = serde_json::from_value(latest.payload.clone())
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                records.push((record, latest.sequence_nr));
                if records.len() >= limit {
                    break;
                }
            }
        }
    }
    Ok(records)
}

/// Find one tenant-scoped delivery record by stable identity.
pub async fn find_delivery_record(
    store: &BoxedEventStore,
    tenant: &str,
    delivery_id: &str,
) -> Result<Option<(ReactionDeliveryRecord, u64)>, PersistenceError> {
    let persistence_id = format!("{tenant}:{REACTION_DELIVERY_ENTITY_TYPE}:{delivery_id}");
    let events = store.read_latest_events(&persistence_id, 1).await?;
    let Some(latest) = events.last() else {
        return Ok(None);
    };
    let record: ReactionDeliveryRecord = serde_json::from_value(latest.payload.clone())
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    if record.intent.tenant != tenant || record.intent.delivery_id != delivery_id {
        return Err(PersistenceError::Serialization(
            "delivery journal identity does not match request".to_string(),
        ));
    }
    Ok(Some((record, latest.sequence_nr)))
}

#[cfg(test)]
#[path = "delivery_test.rs"]
mod tests;
