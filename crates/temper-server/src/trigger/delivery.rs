//! Durable reaction delivery identities and lifecycle records (ADR-0158).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::types::ReactionRule;

/// Reserved event-payload field holding intents co-committed with a source event.
pub const REACTION_INTENTS_FIELD: &str = "_temper_reaction_intents_v1";
/// Maximum automatic delivery attempts before transient failure dead-letters.
pub const MAX_AUTOMATIC_ATTEMPTS: u32 = 5;
/// Maximum operator-requested retries for one transient dead letter.
pub const MAX_MANUAL_RETRIES: u32 = 3;

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
}

/// Immutable normalized reaction input committed with the source event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedReactionIntent {
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
    /// Whether the last terminal failure was classified transient.
    pub transient_failure: bool,
    /// Sanitized last failure reason.
    pub last_error: Option<String>,
}

impl ReactionDeliveryRecord {
    /// Create the initial pending record for a committed intent.
    pub fn pending(intent: PersistedReactionIntent) -> Self {
        Self {
            intent,
            status: ReactionDeliveryStatus::Pending,
            attempts: 0,
            manual_retries: 0,
            fencing_token: 0,
            lease_expires_at: None,
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
        if self.attempts >= MAX_AUTOMATIC_ATTEMPTS {
            return Err("automatic delivery attempt budget exhausted".to_string());
        }
        self.attempts += 1;
        self.fencing_token = self.fencing_token.saturating_add(1);
        self.lease_expires_at = Some(now + lease);
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

/// Derive the immutable identity of one logical reaction delivery.
///
/// Length-prefixing each component prevents delimiter ambiguity. The committed
/// source sequence and registry-stable trigger name/index bind retries and
/// restart recovery to exactly one source transition.
#[allow(clippy::too_many_arguments)]
pub fn stable_delivery_id(
    tenant: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    source_action: &str,
    source_sequence: u64,
    trigger_name: &str,
    trigger_index: usize,
) -> String {
    let mut digest = Sha256::new();
    for component in [
        tenant,
        source_entity_type,
        source_entity_id,
        source_action,
        trigger_name,
    ] {
        digest.update(component.len().to_be_bytes());
        digest.update(component.as_bytes());
    }
    digest.update(source_sequence.to_be_bytes());
    digest.update(trigger_index.to_be_bytes());
    format!("reaction-v1-{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::{
        PersistedReactionIntent, REACTION_INTENTS_FIELD, ReactionDeliveryRecord,
        ReactionDeliveryStatus, attach_intents, extract_intents, stable_delivery_id,
    };
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;

    fn intent() -> PersistedReactionIntent {
        PersistedReactionIntent {
            delivery_id: "reaction-v1-a".to_string(),
            root_delivery_id: "reaction-v1-a".to_string(),
            tenant: "tenant-a".to_string(),
            source_entity_type: "Order".to_string(),
            source_entity_id: "order-7".to_string(),
            source_action: "Confirm".to_string(),
            source_sequence: 42,
            source_to_state: "Confirmed".to_string(),
            source_fields: json!({"payment_id": "payment-9"}),
            trigger_name: "create-payment".to_string(),
            trigger_index: 0,
            depth: 0,
            rule: json!({"name": "create-payment"}),
            authority: json!({"principal": {"id": "User::alice"}}),
            created_at: Utc.timestamp_opt(1_800_000_000, 0).single().unwrap(),
        }
    }

    #[test]
    fn delivery_identity_is_stable_and_binds_source_sequence_and_trigger() {
        let first = stable_delivery_id(
            "tenant-a",
            "Order",
            "order-7",
            "Confirm",
            42,
            "create-payment",
            0,
        );
        let repeated = stable_delivery_id(
            "tenant-a",
            "Order",
            "order-7",
            "Confirm",
            42,
            "create-payment",
            0,
        );
        let next_sequence = stable_delivery_id(
            "tenant-a",
            "Order",
            "order-7",
            "Confirm",
            43,
            "create-payment",
            0,
        );
        let next_trigger = stable_delivery_id(
            "tenant-a",
            "Order",
            "order-7",
            "Confirm",
            42,
            "audit-order",
            1,
        );

        assert_eq!(first, repeated);
        assert_ne!(first, next_sequence);
        assert_ne!(first, next_trigger);
        assert!(first.starts_with("reaction-v1-"));
        assert_eq!(first.len(), "reaction-v1-".len() + 64);
    }

    #[test]
    fn intents_round_trip_inside_the_atomic_source_event_payload() {
        let mut payload = json!({"action": "Confirm", "params": {}});
        attach_intents(&mut payload, std::slice::from_ref(&intent())).unwrap();

        assert!(payload.get(REACTION_INTENTS_FIELD).is_some());
        assert_eq!(extract_intents(&payload).unwrap(), vec![intent()]);
    }

    #[test]
    fn lifecycle_uses_fenced_leases_and_bounds_manual_retry() {
        let now = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
        let mut delivery = ReactionDeliveryRecord::pending(intent());

        let first_fence = delivery.claim(now, Duration::seconds(30)).unwrap();
        assert_eq!(first_fence, 1);
        assert_eq!(delivery.status, ReactionDeliveryStatus::Claimed);
        assert!(delivery.claim(now, Duration::seconds(30)).is_err());

        delivery.recover_expired_lease(now + Duration::seconds(31));
        assert_eq!(delivery.status, ReactionDeliveryStatus::Pending);
        let second_fence = delivery
            .claim(now + Duration::seconds(31), Duration::seconds(30))
            .unwrap();
        assert_eq!(second_fence, 2);
        assert!(delivery.begin_dispatch(first_fence).is_err());
        delivery.begin_dispatch(second_fence).unwrap();
        delivery
            .dead_letter(second_fence, true, "temporary outage")
            .unwrap();

        for expected in 1..=3 {
            assert_eq!(delivery.request_manual_retry().unwrap(), expected);
            delivery.status = ReactionDeliveryStatus::DeadLettered;
            delivery.transient_failure = true;
        }
        assert!(delivery.request_manual_retry().is_err());
    }
}
