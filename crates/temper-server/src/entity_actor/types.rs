//! Types for the entity actor: messages, state, events, and responses.

use std::collections::{BTreeMap, VecDeque};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use temper_runtime::actor::Message;
use temper_runtime::plug::RuntimeRequest;

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
        /// Digest of the exact local state used for an external Cedar
        /// decision. Internal dispatches omit it.
        expected_authorization_precondition: Option<String>,
    },
    /// Get the current entity state.
    GetState,
    /// Get a specific field value.
    GetField { field: String },
    /// Update entity fields (PATCH: merge, PUT: replace).
    UpdateFields {
        fields: serde_json::Value,
        replace: bool,
        /// Digest of the exact state used for an external authorization
        /// decision. The actor rejects the update if that state changed before
        /// this message reached its mailbox.
        expected_precondition: Option<String>,
    },
    /// Delete this entity.
    Delete {
        /// Digest of the exact local state used for an external Cedar
        /// decision. Internal dispatches omit it.
        expected_authorization_precondition: Option<String>,
    },
}

impl Message for EntityMsg {}

impl From<&RuntimeRequest> for EntityMsg {
    fn from(request: &RuntimeRequest) -> Self {
        match request {
            RuntimeRequest::Action {
                name,
                params,
                cross_entity_booleans,
                idempotency_key,
                expected_authorization_precondition,
            } => Self::Action {
                name: name.clone(),
                params: params.clone(),
                cross_entity_booleans: cross_entity_booleans.clone(),
                idempotency_key: idempotency_key.clone(),
                expected_authorization_precondition: expected_authorization_precondition.clone(),
            },
            RuntimeRequest::GetState => Self::GetState,
            RuntimeRequest::GetField { field } => Self::GetField {
                field: field.clone(),
            },
            RuntimeRequest::UpdateFields {
                fields,
                replace,
                expected_precondition,
            } => Self::UpdateFields {
                fields: fields.clone(),
                replace: *replace,
                expected_precondition: expected_precondition.clone(),
            },
            RuntimeRequest::Delete {
                expected_authorization_precondition,
            } => Self::Delete {
                expected_authorization_precondition: expected_authorization_precondition.clone(),
            },
        }
    }
}

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
    pub processed_idempotency_keys: BTreeMap<String, u64>,
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
        if let Some(key) = event.idempotency_key.as_deref() {
            self.record_processed_idempotency_key(key);
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

    fn record_processed_idempotency_key(&mut self, key: &str) {
        let sequence = self.sequence_nr.max(self.total_event_count as u64);
        self.processed_idempotency_keys
            .insert(key.to_string(), sequence);

        while self.processed_idempotency_keys.len() > MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY {
            let Some(oldest_key) = self
                .processed_idempotency_keys
                .iter()
                .min_by_key(|(_, sequence)| **sequence)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.processed_idempotency_keys.remove(&oldest_key);
        }
    }
}

impl temper_jit::apply::EffectTarget for EntityState {
    fn set_status(&mut self, status: String) {
        self.status = status;
    }

    fn add_counter(&mut self, var: &str, amount: usize) {
        *self.counters.entry(var.to_string()).or_default() += amount;
        if var == "items" {
            self.item_count += amount;
        }
    }

    fn sub_counter(&mut self, var: &str, amount: usize) {
        let counter = self.counters.entry(var.to_string()).or_default();
        *counter = counter.saturating_sub(amount);
        if var == "items" {
            self.item_count = self.item_count.saturating_sub(amount);
        }
    }

    fn set_counter(&mut self, var: &str, value: usize) {
        self.counters.insert(var.to_string(), value);
        if var == "items" {
            self.item_count = value;
        }
    }

    fn set_bool(&mut self, var: &str, value: bool) {
        self.booleans.insert(var.to_string(), value);
    }

    fn list_append(&mut self, var: &str, value: String) {
        self.lists.entry(var.to_string()).or_default().push(value);
    }

    fn list_remove_at(&mut self, var: &str, index: usize) {
        let list = self.lists.entry(var.to_string()).or_default();
        if index < list.len() {
            list.remove(index);
        }
    }

    fn store_field_string(&mut self, field: &str, value: String) {
        if let Some(obj) = self.fields.as_object_mut() {
            obj.insert(field.to_string(), serde_json::Value::String(value));
        }
    }

    fn field_value(&self, field: &str) -> Option<serde_json::Value> {
        self.fields.as_object()?.get(field).cloned()
    }

    fn on_skipped_counter(&self, var: &str, param: &str) {
        tracing::warn!(
            entity_type = %self.entity_type,
            entity_id = %self.entity_id,
            counter = %var,
            param = %param,
            "set_counter_from_param skipped because param was missing or not a non-negative integer"
        );
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
#[path = "types_test.rs"]
mod tests;
