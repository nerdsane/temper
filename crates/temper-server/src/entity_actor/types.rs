//! Types for the entity actor: messages, state, events, and responses.

use std::collections::{BTreeMap, VecDeque};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use temper_runtime::actor::Message;

use super::effects::{ScheduledAction, SpawnRequest};
use super::idempotency::{DurableIdempotencyOutcome, idempotency_params_digest};

// TigerStyle: Fixed resource budgets. No unbounded growth.
// These are hard limits, not suggestions. Violations are assertion failures.

/// Maximum unsnapshotted events an actor may replay/hot-hold before refusing new transitions.
pub const MAX_EVENTS_SINCE_SNAPSHOT: usize = 10_000;
/// Backward-compatible alias for older callers; budget enforcement is tail-based.
pub const MAX_EVENTS_PER_ENTITY: usize = MAX_EVENTS_SINCE_SNAPSHOT;
/// Default number of recent events retained in memory per entity.
pub const RECENT_EVENTS_BUDGET_DEFAULT: usize = 50;
/// Maximum items an entity can hold.
pub const MAX_ITEMS_PER_ENTITY: usize = 1_000;
/// Maximum durable idempotency keys retained per entity.
pub const MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY: usize = 1_000;

/// Number of recent events retained in memory per entity.
///
/// Controlled by `TEMPER_RECENT_EVENTS_BUDGET` (default 50).
pub fn recent_events_budget() -> usize {
    static RECENT_EVENTS_BUDGET: OnceLock<usize> = OnceLock::new();
    *RECENT_EVENTS_BUDGET.get_or_init(|| {
        std::env::var("TEMPER_RECENT_EVENTS_BUDGET") // determinism-ok: read once at startup
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(RECENT_EVENTS_BUDGET_DEFAULT)
    })
}

/// Messages the entity actor can receive.
#[derive(Debug)]
pub enum EntityMsg {
    /// Execute a state machine action (e.g., "SubmitOrder", "CancelOrder").
    Action {
        name: String,
        params: serde_json::Value,
        /// Pre-resolved cross-entity state booleans (injected by dispatch layer).
        cross_entity_booleans: BTreeMap<String, bool>,
        /// ADR-0048 sub-decision 5: idempotency key threaded through so the
        /// actor can dedupe against `IdempotencyCache` before executing.
        /// Covers the race where a dispatch-layer retry produces a second
        /// in-flight ask after the first one already processed.
        idempotency_key: Option<String>,
    },
    /// Get the current entity state.
    GetState,
    /// Get a specific field value.
    GetField { field: String },
    /// Update entity fields (PATCH: merge, PUT: replace).
    UpdateFields {
        fields: serde_json::Value,
        replace: bool,
    },
    /// Delete this entity.
    Delete,
}

impl Message for EntityMsg {}

/// The entity's runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityState {
    /// Entity type (e.g., "Order").
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Current status (state machine state).
    pub status: String,
    /// Item count (legacy — prefer `counters["items"]` for new code).
    pub item_count: usize,
    /// Named counter variables (e.g., "items", "review_cycles").
    #[serde(default)]
    pub counters: BTreeMap<String, usize>,
    /// Named boolean variables (e.g., "assignee_set", "has_address").
    #[serde(default)]
    pub booleans: BTreeMap<String, bool>,
    /// Named list variables (e.g., "tags", "approvers").
    #[serde(default)]
    pub lists: BTreeMap<String, Vec<String>>,
    /// All entity fields as a JSON object.
    pub fields: serde_json::Value,
    /// Recent event log (bounded in-memory history for observability).
    #[serde(default)]
    pub events: VecDeque<EntityEvent>,
    /// Total event count ever applied to this entity.
    #[serde(default)]
    pub total_event_count: usize,
    /// Number of events applied after the latest durable snapshot boundary.
    #[serde(default)]
    pub events_since_snapshot: usize,
    /// Sequence number captured by the latest durable snapshot.
    #[serde(default)]
    pub last_snapshot_sequence_nr: u64,
    /// Current event sourcing sequence number (for persistence).
    #[serde(default)]
    pub sequence_nr: u64,
    /// Idempotency keys that have already produced durable events.
    ///
    /// Rebuilt from [`EntityEvent::idempotency_key`] during replay so a retry
    /// after process restart can return success without re-applying the action.
    #[serde(default)]
    pub processed_idempotency_keys: BTreeMap<String, DurableIdempotencyOutcome>,
}

impl EntityState {
    /// Return true if this entity can accept one more event under budget.
    pub fn can_accept_event(&self) -> bool {
        self.events_since_snapshot < MAX_EVENTS_SINCE_SNAPSHOT
    }

    /// Append an event to recent history while enforcing bounded memory.
    pub fn push_event_bounded(&mut self, event: EntityEvent) {
        self.total_event_count = self.total_event_count.saturating_add(1);
        self.events_since_snapshot = self.events_since_snapshot.saturating_add(1);
        if event.idempotency_key.is_some() {
            self.record_processed_idempotency_event(&event);
        }
        self.events.push_back(event);

        let budget = recent_events_budget();
        while self.events.len() > budget {
            self.events.pop_front();
        }
    }

    pub fn has_processed_idempotency_key(&self, key: &str) -> bool {
        self.processed_idempotency_keys.contains_key(key)
    }

    /// Get the durable outcome for an already processed idempotency key.
    pub fn processed_idempotency_outcome(&self, key: &str) -> Option<&DurableIdempotencyOutcome> {
        self.processed_idempotency_keys.get(key)
    }

    fn record_processed_idempotency_event(&mut self, event: &EntityEvent) {
        let key = event
            .idempotency_key
            .as_ref()
            .expect("caller checks idempotency key presence");
        let sequence = self.sequence_nr.max(self.total_event_count as u64);
        self.processed_idempotency_keys.insert(
            key.to_string(),
            DurableIdempotencyOutcome::Completed {
                sequence_nr: sequence,
                action: event.action.clone(),
                params_digest: Some(idempotency_params_digest(&event.params)),
                effect_receipt_version: event.effect_receipt_version,
                custom_effects: event.custom_effects.clone(),
                scheduled_actions: event.scheduled_actions.clone(),
                spawn_requests: event.spawn_requests.clone(),
            },
        );

        while self.processed_idempotency_keys.len() > MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY {
            let Some(oldest_key) = self
                .processed_idempotency_keys
                .iter()
                .min_by_key(|(_, outcome)| outcome.sequence_nr())
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.processed_idempotency_keys.remove(&oldest_key);
        }
    }
}

/// A recorded state transition event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityEvent {
    /// The action that triggered the transition.
    pub action: String,
    /// The status before the transition.
    pub from_status: String,
    /// The status after the transition.
    pub to_status: String,
    /// When the transition occurred.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Parameters passed with the action.
    pub params: serde_json::Value,
    /// Ordered custom effects emitted by the committed transition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_effects: Vec<String>,
    /// Content identity of the exact transition definition that produced the
    /// persisted effect outputs. `None` means a legacy event had no explicit
    /// receipt; `Some` with empty effect vectors is an authoritative empty
    /// receipt and must never be re-derived from the current transition table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_receipt_version: Option<[u8; 32]>,
    /// Ordered scheduled actions emitted by the committed transition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduled_actions: Vec<ScheduledAction>,
    /// Ordered child-spawn requests emitted by the committed transition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawn_requests: Vec<SpawnRequest>,
    /// Optional idempotency key that caused this transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// Default value for `spec_governed`: actions are spec-governed unless explicitly marked otherwise.
fn default_spec_governed() -> bool {
    true
}
/// Serde skip predicate: skip serializing `spec_governed` when it is `true` (the default).
fn is_true(v: &bool) -> bool {
    *v
}

/// The response returned from an action or query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityResponse {
    /// Whether the action succeeded.
    pub success: bool,
    /// The current entity state after the action.
    pub state: EntityState,
    /// Error message if the action failed.
    pub error: Option<String>,
    /// Custom effects emitted during this transition (for hook dispatch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_effects: Vec<String>,
    /// Scheduled actions to fire after delays (for timer dispatch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduled_actions: Vec<crate::entity_actor::effects::ScheduledAction>,
    /// Spawn requests for child entities (executed by dispatch pipeline).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawn_requests: Vec<crate::entity_actor::effects::SpawnRequest>,
    /// Whether the action was governed by a state-machine spec. Defaults to `true`.
    #[serde(default = "default_spec_governed", skip_serializing_if = "is_true")]
    pub spec_governed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entity_state_round_trip() {
        let state = EntityState {
            entity_type: "Order".to_string(),
            entity_id: "order-1".to_string(),
            status: "Draft".to_string(),
            item_count: 2,
            counters: BTreeMap::from([("items".to_string(), 2)]),
            booleans: BTreeMap::from([("assigned".to_string(), true)]),
            lists: BTreeMap::new(),
            fields: json!({"title": "Test Order"}),
            events: VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };
        let serialized = serde_json::to_string(&state).unwrap();
        let deserialized: EntityState = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.entity_type, "Order");
        assert_eq!(deserialized.status, "Draft");
        assert_eq!(deserialized.item_count, 2);
        assert_eq!(deserialized.counters["items"], 2);
        assert!(deserialized.booleans["assigned"]);
    }

    #[test]
    fn entity_state_defaults_on_missing_fields() {
        let json = json!({
            "entity_type": "Task",
            "entity_id": "task-1",
            "status": "Open",
            "item_count": 0,
            "fields": {},
            "events": [],
        });
        let state: EntityState = serde_json::from_value(json).unwrap();
        assert!(state.counters.is_empty());
        assert!(state.booleans.is_empty());
        assert!(state.lists.is_empty());
        assert_eq!(state.total_event_count, 0);
        assert_eq!(state.events_since_snapshot, 0);
        assert_eq!(state.last_snapshot_sequence_nr, 0);
        assert_eq!(state.sequence_nr, 0);
    }

    #[test]
    fn legacy_sequence_only_idempotency_snapshot_still_deserializes() {
        let state: EntityState = serde_json::from_value(json!({
            "entity_type": "Task",
            "entity_id": "task-legacy",
            "status": "Done",
            "item_count": 0,
            "fields": {},
            "events": [],
            "processed_idempotency_keys": {"legacy-key": 7}
        }))
        .unwrap();

        assert!(matches!(
            state.processed_idempotency_outcome("legacy-key"),
            Some(DurableIdempotencyOutcome::LegacySequence(7))
        ));
    }

    #[test]
    fn pushed_event_records_action_and_ordered_custom_effects() {
        let mut state: EntityState = serde_json::from_value(json!({
            "entity_type": "GovernanceDecision",
            "entity_id": "gd-1",
            "status": "Pending",
            "item_count": 0,
            "fields": {},
            "events": []
        }))
        .unwrap();
        state.push_event_bounded(EntityEvent {
            action: "Approve".to_string(),
            from_status: "Pending".to_string(),
            to_status: "Approved".to_string(),
            timestamp: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            params: json!({}),
            custom_effects: vec![
                "GenerateCedarPolicy".to_string(),
                "DispatchCallback".to_string(),
            ],
            effect_receipt_version: Some([1; 32]),
            scheduled_actions: vec![],
            spawn_requests: vec![],
            idempotency_key: Some("approval-1".to_string()),
        });

        assert!(matches!(
            state.processed_idempotency_outcome("approval-1"),
            Some(DurableIdempotencyOutcome::Completed {
                action,
                custom_effects,
                ..
            }) if action == "Approve"
                && custom_effects == &["GenerateCedarPolicy", "DispatchCallback"]
        ));
        let round_trip: EntityState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(
            round_trip.processed_idempotency_outcome("approval-1"),
            state.processed_idempotency_outcome("approval-1")
        );
    }

    #[test]
    fn event_budget_is_based_on_snapshot_tail_not_lifetime_total() {
        let state = EntityState {
            entity_type: "Workspace".to_string(),
            entity_id: "workspace-1".to_string(),
            status: "Active".to_string(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: json!({}),
            events: VecDeque::new(),
            total_event_count: MAX_EVENTS_SINCE_SNAPSHOT + 50,
            events_since_snapshot: 2,
            last_snapshot_sequence_nr: MAX_EVENTS_SINCE_SNAPSHOT as u64 + 48,
            sequence_nr: MAX_EVENTS_SINCE_SNAPSHOT as u64 + 50,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        assert!(state.can_accept_event());
    }

    #[test]
    fn event_budget_rejects_when_snapshot_tail_reaches_cap() {
        let state = EntityState {
            entity_type: "Workspace".to_string(),
            entity_id: "workspace-1".to_string(),
            status: "Active".to_string(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: json!({}),
            events: VecDeque::new(),
            total_event_count: MAX_EVENTS_SINCE_SNAPSHOT,
            events_since_snapshot: MAX_EVENTS_SINCE_SNAPSHOT,
            last_snapshot_sequence_nr: 0,
            sequence_nr: MAX_EVENTS_SINCE_SNAPSHOT as u64,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        assert!(!state.can_accept_event());
    }

    #[test]
    fn entity_response_spec_governed_default() {
        let json = json!({
            "success": true,
            "state": {
                "entity_type": "Order",
                "entity_id": "o1",
                "status": "Draft",
                "item_count": 0,
                "fields": {},
                "events": [],
            },
            "error": null,
        });
        let resp: EntityResponse = serde_json::from_value(json).unwrap();
        assert!(resp.spec_governed); // default is true
    }

    #[test]
    fn entity_response_spec_governed_skipped_when_true() {
        let state = EntityState {
            entity_type: "Order".to_string(),
            entity_id: "o1".to_string(),
            status: "Draft".to_string(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: json!({}),
            events: VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };
        let resp = EntityResponse {
            success: true,
            state,
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        };
        let serialized = serde_json::to_string(&resp).unwrap();
        assert!(!serialized.contains("spec_governed"));
    }

    #[test]
    fn default_spec_governed_is_true() {
        assert!(default_spec_governed());
    }

    #[test]
    fn is_true_helper() {
        assert!(is_true(&true));
        assert!(!is_true(&false));
    }
}
