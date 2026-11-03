//! Types for the entity actor: messages, state, events, and responses.

use std::collections::{BTreeMap, VecDeque};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use temper_runtime::actor::Message;

pub use super::effect_outcome::EffectOutcome;
use super::{EventOrigin, LastEvent};

// Runtime resource budgets. Domain counter constraints belong to the spec.

/// Maximum unsnapshotted events an actor may replay/hot-hold before refusing new transitions.
pub const MAX_EVENTS_SINCE_SNAPSHOT: usize = 10_000;
/// Backward-compatible alias for older callers; budget enforcement is tail-based.
pub const MAX_EVENTS_PER_ENTITY: usize = MAX_EVENTS_SINCE_SNAPSHOT;
/// Default number of recent events retained in memory per entity.
pub const RECENT_EVENTS_BUDGET_DEFAULT: usize = 50;
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
        /// Related entities' statuses the action's guards read (resolved by the dispatch layer).
        related: temper_jit::table::RelatedMap,
        /// ADR-0048 sub-decision 5: idempotency key threaded through so the
        /// actor can dedupe against `IdempotencyCache` before executing.
        /// Covers the race where a dispatch-layer retry produces a second
        /// in-flight ask after the first one already processed.
        idempotency_key: Option<crate::idempotency::IdempotencyKey>,
        /// Digest of the exact local state used for an external Cedar
        /// decision. Internal dispatches omit it.
        expected_authorization_precondition: Option<String>,
        /// Who sent the action. A scheduled action the transition arms keeps
        /// this identity in its row and fires as it. None: the action arms
        /// with no identity (internal callers and tests).
        caller: Option<std::sync::Arc<crate::request_context::AgentContext>>,
    },
    /// Get the current entity state.
    GetState,
    /// Update entity fields (PATCH: merge, PUT: replace).
    UpdateFields {
        /// Kernel dispatch context; never populated from field values.
        caller: Option<std::sync::Arc<crate::request_context::AgentContext>>,
        fields: serde_json::Value,
        replace: bool,
        /// Digest of the exact state used for an external authorization
        /// decision. The actor rejects the update if that state changed before
        /// this message reached its mailbox.
        expected_precondition: Option<String>,
    },
    /// Record that a spawn this entity's transition made could not create its
    /// child: journal it, and clear the spawn field only if this reservation
    /// still owns it and the child is known to be absent.
    RecordSpawnRefusal {
        /// The refused spawn.
        refusal: super::spawn_refusal::SpawnRefusal,
    },
    /// Record that this entity's state timer stopped because nothing names
    /// its caller, for the action no one could make.
    RecordTimerNeedsPerson {
        /// The refused action.
        action: String,
        /// The journal sequence the stop was decided at. A record that moved
        /// past it is left as it is: the newer move decides its timer.
        checked: u64,
    },
    /// Delete this entity.
    Delete {
        /// Kernel dispatch context; never populated from field values.
        caller: Option<std::sync::Arc<crate::request_context::AgentContext>>,
        /// Digest of the exact local state used for an external Cedar
        /// decision. Internal dispatches omit it.
        expected_authorization_precondition: Option<String>,
    },
}

impl EntityMsg {
    /// The message as its record's logs name it: an action's name, or the kind
    /// of message.
    pub(crate) fn name(&self) -> &str {
        match self {
            EntityMsg::Action { name, .. } => name,
            EntityMsg::GetState => "GetState",
            EntityMsg::UpdateFields { .. } => "UpdateFields",
            EntityMsg::RecordSpawnRefusal { .. } => "RecordSpawnRefusal",
            EntityMsg::RecordTimerNeedsPerson { .. } => "RecordTimerNeedsPerson",
            EntityMsg::Delete { .. } => "Delete",
        }
    }
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
    /// The spawn reservation currently owning each parent field. Identity is
    /// the committing event sequence and the spawn position within that event.
    /// Kept in snapshots; absent in snapshots written before reservations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub spawn_reservations: BTreeMap<String, String>,
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
    /// Idempotency keys that have already produced durable events, with the
    /// action each was used for.
    ///
    /// Rebuilt from [`EntityEvent::idempotency_key`] during replay so a retry
    /// after process restart can return success without re-applying the action.
    #[serde(default)]
    pub processed_idempotency_keys: BTreeMap<String, ProcessedIdempotencyKey>,
    /// When the current status began, and when each action last ran since.
    /// Kept in snapshots, which leave `events` out.
    #[serde(default)]
    pub status_clock: StatusClock,
    /// Newest event's time and provenance, independent of bounded history.
    #[serde(default, flatten)]
    pub last_event: LastEvent,
}

/// A durable idempotency key: when it was recorded and the action it was for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredIdempotencyKey")]
pub struct ProcessedIdempotencyKey {
    /// Sequence position when the key was recorded; the oldest goes first.
    pub sequence: u64,
    /// The action the key was used for. `None` for a key restored from a
    /// snapshot written before actions were kept with keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

impl ProcessedIdempotencyKey {
    /// Whether a request for `action` is a retry of the one this key recorded.
    pub fn was_for(&self, action: &str) -> bool {
        self.action
            .as_deref()
            .is_none_or(|recorded| recorded == action)
    }
}

/// How a durable key is stored: a bare sequence in older snapshots.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredIdempotencyKey {
    Recorded {
        sequence: u64,
        #[serde(default)]
        action: Option<String>,
    },
    Sequence(u64),
}

impl From<StoredIdempotencyKey> for ProcessedIdempotencyKey {
    fn from(stored: StoredIdempotencyKey) -> Self {
        match stored {
            StoredIdempotencyKey::Recorded { sequence, action } => Self { sequence, action },
            StoredIdempotencyKey::Sequence(sequence) => Self {
                sequence,
                action: None,
            },
        }
    }
}

/// When an entity's current status began, and when each action was last
/// applied since then: how long a state timer's clock has been running. A state
/// restored from a snapshot has no `events` before the snapshot to read this
/// from, so it is kept here instead (ADR-0056).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusClock {
    /// When the event that entered the current status was applied.
    #[serde(default)]
    pub entered_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The last time each action was applied since the status was entered.
    #[serde(default)]
    pub actions: BTreeMap<String, chrono::DateTime<chrono::Utc>>,
}

impl StatusClock {
    fn record(&mut self, event: &EntityEvent) {
        if event.from_status != event.to_status {
            self.entered_at = Some(event.timestamp);
            self.actions.clear();
        } else {
            self.actions.insert(event.action.clone(), event.timestamp);
        }
    }
}

impl EntityState {
    /// Return true if this entity can accept one more event under budget.
    pub fn can_accept_event(&self) -> bool {
        self.events_since_snapshot < MAX_EVENTS_SINCE_SNAPSHOT
    }

    /// Append an event to recent history while enforcing bounded memory.
    pub fn push_event_bounded(&mut self, event: EntityEvent) {
        if self.last_event.first_event_at.is_none()
            && self.total_event_count == 0
            && self.sequence_nr <= 1
        {
            self.last_event.first_event_at = Some(event.timestamp);
        }
        self.total_event_count = self.total_event_count.saturating_add(1);
        self.events_since_snapshot = self.events_since_snapshot.saturating_add(1);
        if let Some(key) = event.idempotency_key.as_deref() {
            self.record_processed_idempotency_key(key, &event.action);
        }
        self.status_clock.record(&event);
        self.last_event.record(&event);
        self.events.push_back(event);

        let budget = recent_events_budget();
        while self.events.len() > budget {
            self.events.pop_front();
        }
    }

    /// Where this state is in the record's history. With a journal that is the
    /// journal sequence; without one the sequence stays put, and the count of
    /// events applied moves instead. Both only grow while the record lives.
    pub fn event_sequence(&self) -> u64 {
        self.sequence_nr.max(self.total_event_count as u64)
    }

    /// Whether `key` has already produced a durable event on this entity.
    pub fn has_processed_idempotency_key(&self, key: &str) -> bool {
        self.processed_idempotency_keys.contains_key(key)
    }

    fn record_processed_idempotency_key(&mut self, key: &str, action: &str) {
        let sequence = self.event_sequence();
        self.processed_idempotency_keys.insert(
            key.to_string(),
            ProcessedIdempotencyKey {
                sequence,
                action: Some(action.to_string()),
            },
        );

        while self.processed_idempotency_keys.len() > MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY {
            let Some(oldest_key) = self
                .processed_idempotency_keys
                .iter()
                .min_by_key(|(_, recorded)| recorded.sequence)
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
    /// Kernel-captured immediate source and authenticated request subject.
    #[serde(default, flatten)]
    pub origin: EventOrigin,
    /// Optional idempotency key that caused this transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Which kind of event the kernel wrote. Only the kernel's delete writes a
    /// tombstone, whatever a spec names its actions.
    #[serde(default)]
    pub kind: temper_runtime::persistence::EventKind,
    /// What the transition's effects wrote, recorded when it ran. Replay
    /// restores it instead of running the effects again. `None` on events
    /// that run no effects (creation, field updates, tombstones) and on
    /// transitions journaled before outcomes were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<EffectOutcome>,
}

impl EntityEvent {
    /// The `Created` event that starts a journal, carrying no params.
    pub(crate) fn bootstrap(to_status: String) -> Self {
        Self {
            action: "Created".to_string(),
            from_status: String::new(),
            to_status,
            timestamp: temper_runtime::scheduler::sim_now(),
            params: serde_json::json!({}),
            idempotency_key: None,
            origin: crate::entity_actor::EventOrigin::internal(),
            kind: temper_runtime::persistence::EventKind::Transition,
            outcome: None,
        }
    }
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
    /// Why the action was refused, as typed data beside `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<temper_jit::table::Refusal>,
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
    /// Whether the action was refused before anything was written: its guard,
    /// its state or its inputs did not allow it, the record had moved on and
    /// its journal, read again, does not hold the action, or the event was too
    /// large. A failed answer that is not a refusal (storage failed, a
    /// concurrency violation that was not resolved by reading the journal, or
    /// a step after the commit failed) may have landed. `refusal` says why a
    /// guard, state or input refused it; it can be set when this is not, for a
    /// replay refused after a concurrency violation that an earlier attempt of
    /// the same action may have caused.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refused: bool,
    /// The action this answer is for, when it is not the one requested: a
    /// follow-up's retry that named another action is answered by the
    /// outcome its key recorded (`IdempotencyKey::FollowUp`), and the
    /// effects and reactions that follow belong to that recorded action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_action: Option<String>,
    /// The triggers of this committed action that failed, or whose answer
    /// was refused, each as `trigger: error`. The action stands: they are
    /// reported beside its success, and the record shows each failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trigger_failures: Vec<String>,
    /// Whether the store could not take the action's write: it failed, or its
    /// disk is under the floor. Answered 503, so the caller tries again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub store_unavailable: bool,
    /// When the move was made, for a move that wrote no event: a machine made
    /// it and it left its record as it was (`entity_actor::unrecorded`). None
    /// for every other answer, whose move, if any, is its newest event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unrecorded_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl EntityResponse {
    /// The action this answer is for: `requested`, unless a follow-up's key
    /// recorded another ([`Self::answered_action`]).
    pub fn action<'a>(&'a self, requested: &'a str) -> &'a str {
        self.answered_action.as_deref().unwrap_or(requested)
    }

    /// The state this successful answer's move started from, and when it was
    /// made: its event's, or, for a move that wrote none
    /// ([`Self::unrecorded_at`]), the record's status, which the move left as
    /// it was, and the move's own time. None when the record has no event.
    pub(crate) fn move_start(&self) -> Option<(&str, chrono::DateTime<chrono::Utc>)> {
        match self.unrecorded_at {
            Some(at) => Some((self.state.status.as_str(), at)),
            None => self
                .state
                .events
                .back()
                .map(|event| (event.from_status.as_str(), event.timestamp)),
        }
    }

    /// The answer to an action refused before anything was written.
    pub(crate) fn refused(state: EntityState, error: String) -> Self {
        Self {
            refused: true,
            store_unavailable: false,
            ..Self::write_failed(state, error)
        }
    }

    /// The answer to an action the store could not take: see `store_unavailable`.
    pub(crate) fn store_unavailable(state: EntityState, error: String) -> Self {
        Self {
            store_unavailable: true,
            ..Self::write_failed(state, error)
        }
    }

    /// The answer to a write decided on a state its record has since left:
    /// nothing was written, and `state` is the record as it is now.
    pub(crate) fn record_changed(state: EntityState, error: String) -> Self {
        Self {
            refusal: Some(temper_jit::table::Refusal::RecordChanged),
            ..Self::refused(state, error)
        }
    }

    /// Whether this answers a write decided on a state its record has since
    /// left ([`Self::record_changed`]).
    pub(crate) fn is_record_changed(&self) -> bool {
        self.refused && self.refusal == Some(temper_jit::table::Refusal::RecordChanged)
    }

    /// The answer to an action whose write failed, so it may have landed.
    pub(crate) fn write_failed(state: EntityState, error: String) -> Self {
        Self {
            success: false,
            state,
            error: Some(error),
            refusal: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
            refused: false,
            answered_action: None,
            trigger_failures: Vec::new(),
            store_unavailable: false,
            unrecorded_at: None,
        }
    }
}

#[path = "module_record.rs"]
mod module_record;

#[cfg(test)]
#[path = "types_test.rs"]
mod tests;
