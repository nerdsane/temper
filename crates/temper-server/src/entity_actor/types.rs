//! Types for the entity actor: messages, state, events, and responses.

use std::collections::{BTreeMap, VecDeque};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use temper_runtime::actor::Message;

// TigerStyle: Fixed resource budgets. No unbounded growth.
// These are hard limits, not suggestions. Violations are assertion failures.

/// Maximum events per entity before the actor refuses new transitions.
pub const MAX_EVENTS_PER_ENTITY: usize = 10_000;
/// Default number of recent events retained in memory per entity.
pub const RECENT_EVENTS_BUDGET_DEFAULT: usize = 50;
/// Maximum items an entity can hold.
pub const MAX_ITEMS_PER_ENTITY: usize = 1_000;

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
    /// Current event sourcing sequence number (for persistence).
    #[serde(default)]
    pub sequence_nr: u64,
}

impl EntityState {
    /// Return true if this entity can accept one more event under budget.
    pub fn can_accept_event(&self) -> bool {
        self.total_event_count < MAX_EVENTS_PER_ENTITY
    }

    /// Append an event to recent history while enforcing bounded memory.
    pub fn push_event_bounded(&mut self, event: EntityEvent) {
        self.total_event_count = self.total_event_count.saturating_add(1);
        self.events.push_back(event);

        let budget = recent_events_budget();
        while self.events.len() > budget {
            self.events.pop_front();
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
            sequence_nr: 0,
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
        assert_eq!(state.sequence_nr, 0);
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
            sequence_nr: 0,
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
