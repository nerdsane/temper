//! Idempotency cache for deduplicating agent retries.
//!
//! Per-entity-actor LRU cache of recent `Idempotency-Key` → `EntityResponse`.
//! Entries expire after `IDEMPOTENCY_TTL_SECS` and are evicted when the
//! per-actor budget is exceeded.
//!
//! Every entry is bound to the canonical request that produced it
//! ([`request_binding`], ADR-0182). Reusing a key for a different action or
//! body is a [`IdempotencyLookup::Mismatch`], never a cached success.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};
use temper_runtime::scheduler::sim_now;

use crate::entity_actor::EntityResponse;

/// Actor reply error for a key reused with a different action or body.
/// Dispatch maps it to `DispatchError::IdempotencyKeyMismatch` (HTTP 422).
pub const IDEMPOTENCY_KEY_MISMATCH: &str = "IdempotencyKeyMismatch: idempotency key was already used for a different action or request body";

/// Actor reply error for a processed key whose original request cannot be
/// verified from the journal. Dispatch maps it to
/// `DispatchError::IdempotencyKeyUnverifiable` (HTTP 409).
pub const IDEMPOTENCY_KEY_UNVERIFIABLE: &str = "IdempotencyKeyUnverifiable: idempotency key was already used but its original request cannot be verified";

/// Actor reply error when the request matches, but its logical reply or required
/// completion is unproven. The transport code remains IdempotencyKeyUnverifiable.
pub const IDEMPOTENCY_REPLY_UNVERIFIABLE: &str = "IdempotencyKeyUnverifiable: committed request matches, but its original logical reply or completion cannot be verified";

const REQUEST_BINDING_TAG: &[u8] = b"temper.idempotency.v1";

/// Canonical binding of an action request (ADR-0182).
///
/// SHA-256 over the unqualified action name and the canonical JSON of the
/// params. Only top-level server-derived keys (the ones dispatch strips before
/// journaling, see `sanitize_action_params`) are removed; nested objects are
/// hashed with sorted keys, arrays keep their order, and nested keys of any name
/// are part of the binding. Transport metadata is never an input.
pub fn request_binding(action: &str, params: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    update_bytes(&mut hasher, REQUEST_BINDING_TAG);
    update_bytes(&mut hasher, unqualified_action(action).as_bytes());
    update_json(&mut hasher, params, true);
    hex_digest(hasher)
}

const RESULT_DIGEST_TAG: &[u8] = b"temper.idempotency.result.v1";

/// Immutable execution provenance of a keyed commit (ADR-0182, review
/// correction 2): SHA-256 over the post-commit logical state — `status`,
/// `item_count`, `counters`, `booleans`, `lists` and `fields`. Bookkeeping
/// (event history, sequence numbers, snapshot counters, processed keys) is
/// excluded, so a live commit and a faithful replay of it hash equal, while a
/// replay under changed transition rules does not.
pub fn result_digest(state: &crate::entity_actor::EntityState) -> String {
    let logical = serde_json::json!({
        "status": state.status,
        "item_count": state.item_count,
        "counters": state.counters,
        "booleans": state.booleans,
        "lists": state.lists,
        "fields": state.fields,
    });
    let mut hasher = Sha256::new();
    update_bytes(&mut hasher, RESULT_DIGEST_TAG);
    update_json(&mut hasher, &logical, false);
    hex_digest(hasher)
}

/// Stamp an atomic composite sub-write with binding, state and reply provenance.
/// `state` is the post-commit state, not proof of the required post-append
/// projection. Such commits cannot certify a cold logical reply.
pub fn stamp_keyed_commit(
    event: &mut crate::entity_actor::EntityEvent,
    key: &str,
    action: &str,
    params: &serde_json::Value,
    state: &crate::entity_actor::EntityState,
) {
    stamp_keyed_reply(
        event,
        key,
        action,
        params,
        state,
        ActionReplyMode::Composite,
    );
}

/// Stamp the trusted first reply boundary in the same journal event as the commit.
pub(crate) fn stamp_keyed_reply(
    event: &mut crate::entity_actor::EntityEvent,
    key: &str,
    action: &str,
    params: &serde_json::Value,
    state: &crate::entity_actor::EntityState,
    mode: ActionReplyMode,
) {
    event.idempotency_key = Some(key.to_string());
    let binding = request_binding(action, params);
    event.idempotency_binding = Some(binding.clone());
    let core = result_digest(state);
    event.idempotency_reply = Some(reply_proof::stamp(key, &binding, &core, mode));
    event.idempotency_result = Some(core);
}

fn hex_digest(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn update_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    hasher.update(len.to_be_bytes());
    hasher.update(bytes);
}

/// Canonical JSON: sorted object keys, ordered arrays, type-tagged and
/// length-prefixed scalars. `top_level` drops server-derived keys of the
/// outermost object only.
fn update_json(hasher: &mut Sha256, value: &serde_json::Value, top_level: bool) {
    match value {
        serde_json::Value::Null => hasher.update([0]),
        serde_json::Value::Bool(value) => hasher.update([1, u8::from(*value)]),
        serde_json::Value::Number(value) => {
            hasher.update([2]);
            update_bytes(hasher, value.to_string().as_bytes());
        }
        serde_json::Value::String(value) => {
            hasher.update([3]);
            update_bytes(hasher, value.as_bytes());
        }
        serde_json::Value::Array(values) => {
            hasher.update([4]);
            hasher.update((values.len() as u64).to_be_bytes());
            for value in values {
                update_json(hasher, value, false);
            }
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map
                .keys()
                .filter(|key| {
                    !(top_level && temper_spec::automaton::is_server_derived_field_name(key))
                })
                .collect();
            keys.sort();
            hasher.update([5]);
            hasher.update((keys.len() as u64).to_be_bytes());
            for key in keys {
                update_bytes(hasher, key.as_bytes());
                update_json(hasher, &map[key.as_str()], false);
            }
        }
    }
}

/// Strip an OData namespace qualifier (`Temper.Example.AddItem` → `AddItem`).
pub fn unqualified_action(action: &str) -> &str {
    action.rsplit('.').next().unwrap_or(action)
}

/// Result of a bound idempotency lookup.
#[derive(Debug)]
pub enum IdempotencyLookup {
    /// No live entry for this key.
    Miss,
    /// The key was used for this exact request; the original response.
    Hit(Box<EntityResponse>),
    /// The key was used for a different action or body.
    Mismatch,
}

/// Maximum number of idempotency entries per actor (TigerStyle budget).
pub const IDEMPOTENCY_BUDGET_PER_ACTOR: usize = 1_000;

/// Time-to-live for idempotency entries in seconds.
pub const IDEMPOTENCY_TTL_SECS: i64 = 3600;

/// Root continuation capacity; descendants share their root's lease.
const ROOT_COMPLETION_BUDGET: usize = 64;

/// A cached idempotent response.
struct IdempotencyEntry {
    /// The cached response to return on duplicate requests.
    response: EntityResponse,
    /// Canonical request binding ([`request_binding`]) of the first request.
    binding: String,
    /// When this entry was created (for TTL eviction).
    created_at: chrono::DateTime<chrono::Utc>,
    /// Receipt owning the effects and registered reaction obligation.
    effects: EffectsState,
    first_reply_mode: Option<ActionReplyMode>,
}

impl IdempotencyEntry {
    fn last_progress_at(&self) -> chrono::DateTime<chrono::Utc> {
        match &self.effects {
            EffectsState::Operation(receipt) => receipt.finished_at().unwrap_or(self.created_at),
            _ => self.created_at,
        }
    }
}

mod effects;
mod reply_proof;
pub use reply_proof::ActionReplyMode;
mod wait_graph;
use effects::EffectsState;
pub(crate) use effects::{
    CompletionResult, OperationClaim, OperationOwner, OperationPhase, OperationReceipt,
    OperationRequest,
};

/// Per-entity-actor idempotency cache.
///
/// Thread-safe via `RwLock`. Uses `BTreeMap` for deterministic iteration
/// order (DST compliance).
pub struct IdempotencyCache {
    /// actor_key → (idempotency_key → entry).
    entries: RwLock<BTreeMap<String, BTreeMap<String, IdempotencyEntry>>>,
    pub(crate) continuation_slots: Arc<tokio::sync::Semaphore>,
    pub(crate) wait_graph: Arc<wait_graph::WaitGraph>,
    #[cfg(test)]
    pub(crate) missing_receipt_once: std::sync::Mutex<Option<(String, String)>>,
    #[cfg(test)]
    pub(crate) reaction_handoff_once: std::sync::Mutex<Option<(String, String)>>,
    #[cfg(test)]
    pub(crate) interrupt_action_once: std::sync::Mutex<Option<(String, String, String)>>,
}

impl IdempotencyCache {
    /// Create a new empty idempotency cache.
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
            wait_graph: Arc::new(wait_graph::WaitGraph::default()),
            #[cfg(test)]
            missing_receipt_once: std::sync::Mutex::new(None),
            #[cfg(test)]
            reaction_handoff_once: std::sync::Mutex::new(None),
            #[cfg(test)]
            interrupt_action_once: std::sync::Mutex::new(None),
            continuation_slots: Arc::new(tokio::sync::Semaphore::new(ROOT_COMPLETION_BUDGET)),
        }
    }

    /// Look up a cached response bound to `binding`.
    pub fn lookup(&self, actor_key: &str, idem_key: &str, binding: &str) -> IdempotencyLookup {
        self.lookup_inner(actor_key, idem_key, binding, false)
    }

    /// Bound lookup that only reports a hit after the registered operation completes.
    ///
    /// HTTP/OData callers use this stricter lookup so retries join the
    /// registered reaction terminal milestone, rather than short-circuiting
    /// on either a committed actor reply or effects readiness alone. A mismatch is reported regardless
    /// of effect state: the key is bound as soon as the first request succeeds.
    pub fn lookup_after_completion(
        &self,
        actor_key: &str,
        idem_key: &str,
        binding: &str,
    ) -> IdempotencyLookup {
        self.lookup_inner(actor_key, idem_key, binding, true)
    }

    fn lookup_inner(
        &self,
        actor_key: &str,
        idem_key: &str,
        binding: &str,
        require_completion: bool,
    ) -> IdempotencyLookup {
        let now = sim_now();
        let entries = match self.entries.read() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(entry) = entries
            .get(actor_key)
            .and_then(|actor_entries| actor_entries.get(idem_key))
        else {
            return IdempotencyLookup::Miss;
        };

        let age = now.signed_duration_since(entry.last_progress_at());
        if !entry.effects.is_claimed() && age.num_seconds() > IDEMPOTENCY_TTL_SECS {
            return IdempotencyLookup::Miss;
        }
        if entry.binding != binding {
            return IdempotencyLookup::Mismatch;
        }
        if let EffectsState::Operation(receipt) = &entry.effects
            && let Some(response) = receipt.terminal()
        {
            return IdempotencyLookup::Hit(Box::new(response));
        }
        if require_completion {
            return IdempotencyLookup::Miss;
        }
        IdempotencyLookup::Hit(Box::new(entry.response.clone()))
    }

    /// Cache a response for a given actor and idempotency key.
    ///
    /// If the per-actor budget is exceeded, the oldest entry is evicted.
    pub fn put(&self, actor_key: &str, idem_key: &str, binding: &str, response: EntityResponse) {
        self.insert(
            actor_key,
            idem_key,
            binding,
            response,
            EffectsState::Pending,
            None,
        );
    }

    /// Cache a response rebuilt from history (journal replay or the latest
    /// in-memory event). It is not a new commit, so no dispatcher may run its
    /// transition effects.
    pub fn put_historical(
        &self,
        actor_key: &str,
        idem_key: &str,
        binding: &str,
        response: EntityResponse,
    ) {
        self.insert(
            actor_key,
            idem_key,
            binding,
            response,
            EffectsState::Historical,
            None,
        );
    }

    /// Cache the actor commit with its immutable trusted first mode.
    pub(crate) fn put_committed(
        &self,
        actor_key: &str,
        idem_key: &str,
        binding: &str,
        mut response: EntityResponse,
        mode: ActionReplyMode,
    ) {
        let effects = if mode == ActionReplyMode::DirectCore {
            response.custom_effects.clear();
            response.scheduled_actions.clear();
            response.spawn_requests.clear();
            EffectsState::Historical
        } else {
            EffectsState::Pending
        };
        self.insert(actor_key, idem_key, binding, response, effects, Some(mode));
    }

    fn insert(
        &self,
        actor_key: &str,
        idem_key: &str,
        binding: &str,
        response: EntityResponse,
        effects: EffectsState,
        first_reply_mode: Option<ActionReplyMode>,
    ) {
        let now = sim_now();
        let mut entries = self.entries.write().unwrap(); // ci-ok: infallible lock
        let actor_entries = entries.entry(actor_key.to_string()).or_default();

        // Never replace an in-flight owner or disconnect its waiters.
        if actor_entries
            .get(idem_key)
            .is_some_and(|entry| entry.effects.is_claimed())
        {
            return;
        }

        // Evict expired entries first, except active owners.
        actor_entries.retain(|_, entry| {
            entry.effects.is_claimed()
                || now
                    .signed_duration_since(entry.last_progress_at())
                    .num_seconds()
                    <= IDEMPOTENCY_TTL_SECS
        });

        // Budget enforcement: evict oldest if at capacity.
        while actor_entries.len() >= IDEMPOTENCY_BUDGET_PER_ACTOR {
            // Find the oldest entry by created_at.
            if let Some(oldest_key) = actor_entries
                .iter()
                .filter(|(_, entry)| !entry.effects.is_claimed())
                .min_by_key(|(_, e)| e.last_progress_at())
                .map(|(k, _)| k.clone())
            {
                actor_entries.remove(&oldest_key);
            } else {
                // Active dispatches outlive the response-cache budget. Their
                // claims must remain reachable until completion/cancellation.
                tracing::warn!(
                    actor_key,
                    entries = actor_entries.len(),
                    "idempotency cache budget occupied by active effects"
                );
                break;
            }
        }

        actor_entries.insert(
            idem_key.to_string(),
            IdempotencyEntry {
                response,
                binding: binding.to_string(),
                created_at: now,
                effects,
                first_reply_mode,
            },
        );
    }
}

impl Default for IdempotencyCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;

/// Whether durable provenance authorizes the verified actor prefix as a reply.
pub(crate) fn verifies_core_reply(
    event: &crate::entity_actor::EntityEvent,
    binding: &str,
    core: &str,
) -> bool {
    event.idempotency_key.as_deref().is_some_and(|key| {
        reply_proof::verifies_core(event.idempotency_reply.as_ref(), key, binding, core)
    })
}
