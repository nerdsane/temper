//! Idempotency cache for deduplicating agent retries.
//!
//! Recent `Idempotency-Key` → `EntityResponse` replies, kept per entity actor.
//! Entries expire after `IDEMPOTENCY_TTL_SECS`. Each reply remembers the
//! request it answered ([`RequestFingerprint`]): a key answers only a retry of
//! that request, never a different action or body sent with the same key. A
//! follow-up's key ([`IdempotencyKey::FollowUp`]) names its cause, which has
//! one outcome: any retry of the cause is answered by the first, even one
//! that now names another action (an integration that failed where it
//! succeeded).
//!
//! Every dispatched action carries a key, so every write adds a reply. The
//! cache is therefore bounded by the bytes it holds across all actors
//! ([`IDEMPOTENCY_CACHE_BYTES_BUDGET`]), and a cached reply leaves out the two
//! things that dominate a busy entity's state: its recent events' parameters
//! and the actor's durable key set. Replies whose post-dispatch effects have
//! run are evicted first, oldest first: the actor's durable
//! `processed_idempotency_keys` still deduplicates them, without the original
//! reply. Replies still waiting for their effects go last, because a retry
//! needs their effects (spawns, timers, triggers) to run once.

use std::collections::BTreeMap;
use std::sync::RwLock;

use temper_runtime::scheduler::sim_now;

use crate::entity_actor::{EntityResponse, EntityState};

mod effects;
mod reply;
mod request;

pub use effects::{EffectsClaim, EffectsOwner, retry_reply};
pub use request::{IdempotencyKey, RequestFingerprint, derived_idempotency_key};

use reply::{estimated_entry_bytes, replay_copy};

/// Estimated bytes of cached replies held across all actors (TigerStyle budget).
pub const IDEMPOTENCY_CACHE_BYTES_BUDGET: usize = 16 * 1024 * 1024;

/// Time-to-live for idempotency entries in seconds.
pub const IDEMPOTENCY_TTL_SECS: i64 = 3600;

/// A cached idempotent response.
struct IdempotencyEntry {
    /// The request this reply answered.
    request: RequestFingerprint,
    /// The cached response to return on duplicate requests.
    response: EntityResponse,
    /// When this entry was created (for TTL eviction).
    created_at: chrono::DateTime<chrono::Utc>,
    /// Whether dispatcher-side post-dispatch effects have completed for this
    /// cached response.
    effects_applied: bool,
    /// Estimated bytes this entry holds; counts toward the budget.
    bytes: usize,
    /// Position in the cache-wide insertion order.
    seq: u64,
}

impl IdempotencyEntry {
    /// The cached reply as the answer to `request`, a retry of the request it
    /// answered. A follow-up's retry may name another action; the reply then
    /// says which one it is for.
    fn reply_to(&self, request: &RequestFingerprint) -> EntityResponse {
        let mut reply = self.response.clone();
        if self.request.action() != request.action() {
            reply.answered_action = Some(self.request.action().to_string());
        }
        reply
    }
}

/// Insertion order, oldest first: seq → (actor_key, idempotency_key).
type Order = BTreeMap<u64, (String, String)>;

#[derive(Default)]
struct Entries {
    /// actor_key → (idempotency_key → entry).
    by_actor: BTreeMap<String, BTreeMap<String, IdempotencyEntry>>,
    /// Replies whose post-dispatch effects have run; evicted first.
    applied: Order,
    /// Replies still waiting for their post-dispatch effects; evicted last.
    pending: Order,
    next_seq: u64,
    bytes: usize,
}

impl Entries {
    fn len(&self) -> usize {
        self.applied.len() + self.pending.len()
    }

    fn remove(&mut self, actor_key: &str, idem_key: &str) {
        let Some(actor_entries) = self.by_actor.get_mut(actor_key) else {
            return;
        };
        let Some(entry) = actor_entries.remove(idem_key) else {
            return;
        };
        if actor_entries.is_empty() {
            self.by_actor.remove(actor_key);
        }
        self.applied.remove(&entry.seq);
        self.pending.remove(&entry.seq);
        self.bytes -= entry.bytes;
    }

    /// Evict one reply: the oldest whose effects have run, else the oldest.
    fn evict_one(&mut self) -> bool {
        let Some((_, (actor_key, idem_key))) = self
            .applied
            .pop_first()
            .or_else(|| self.pending.pop_first())
        else {
            return false;
        };
        self.remove(&actor_key, &idem_key);
        true
    }

    /// Evict every reply older than the TTL.
    fn evict_expired(&mut self, now: chrono::DateTime<chrono::Utc>) {
        loop {
            let oldest = [
                self.applied.first_key_value(),
                self.pending.first_key_value(),
            ]
            .into_iter()
            .flatten()
            .min_by_key(|(seq, _)| **seq)
            .map(|(_, keys)| keys.clone());
            let Some((actor_key, idem_key)) = oldest else {
                return;
            };
            let expired = self
                .by_actor
                .get(&actor_key)
                .and_then(|actor_entries| actor_entries.get(&idem_key))
                .is_some_and(|entry| is_expired(entry, now));
            if !expired {
                return;
            }
            self.remove(&actor_key, &idem_key);
        }
    }
}

fn is_expired(entry: &IdempotencyEntry, now: chrono::DateTime<chrono::Utc>) -> bool {
    now.signed_duration_since(entry.created_at).num_seconds() > IDEMPOTENCY_TTL_SECS
}

/// Idempotency cache shared by every entity actor.
///
/// Thread-safe via `RwLock`. Uses `BTreeMap` for deterministic iteration
/// order (DST compliance).
pub struct IdempotencyCache {
    entries: RwLock<Entries>,
    budget_bytes: usize,
    effects_owners: effects::EffectsOwners,
}

/// What the cache holds for a key, compared with the request now using it.
#[derive(Debug)]
pub enum CachedReply {
    /// The same request: answer it with this reply. Boxed because a reply
    /// is much larger than the other variants.
    Replay(Box<EntityResponse>),
    /// The key answered a different request, which named this action.
    OtherRequest(String),
    /// Nothing is cached for the key.
    Miss,
}

impl IdempotencyCache {
    /// Create a new empty idempotency cache with the default budget.
    pub fn new() -> Self {
        Self::with_budget_bytes(IDEMPOTENCY_CACHE_BYTES_BUDGET)
    }

    /// Create a new empty idempotency cache holding at most `budget_bytes`.
    pub fn with_budget_bytes(budget_bytes: usize) -> Self {
        Self {
            entries: RwLock::new(Entries::default()),
            budget_bytes,
            effects_owners: effects::EffectsOwners::default(),
        }
    }

    /// Estimated bytes of cached replies currently held.
    pub fn held_bytes(&self) -> usize {
        match self.entries.read() {
            Ok(entries) => entries.bytes,
            Err(poisoned) => poisoned.into_inner().bytes,
        }
    }

    /// Look up the reply cached for `idem_key`, compared with `request`.
    pub fn get(
        &self,
        actor_key: &str,
        idem_key: &str,
        request: &RequestFingerprint,
    ) -> CachedReply {
        self.with_entry(actor_key, idem_key, |entry| {
            if entry.request == *request {
                CachedReply::Replay(Box::new(entry.reply_to(request)))
            } else {
                CachedReply::OtherRequest(entry.request.action().to_string())
            }
        })
        .unwrap_or(CachedReply::Miss)
    }

    /// The reply for a retry of `request`, only once its post-dispatch
    /// effects have run.
    ///
    /// HTTP/OData callers use this stricter lookup so a retry after a dropped
    /// actor reply re-enters dispatch and fires effects instead of short-
    /// circuiting at the protocol boundary. A different request sent with the
    /// key also goes on to dispatch, where the actor refuses it.
    pub fn get_after_effects_applied(
        &self,
        actor_key: &str,
        idem_key: &str,
        request: &RequestFingerprint,
    ) -> Option<EntityResponse> {
        self.with_entry(actor_key, idem_key, |entry| {
            (entry.effects_applied && entry.request == *request).then(|| entry.reply_to(request))
        })
        .flatten()
    }

    /// The cached reply for `idem_key` once its post-dispatch effects have run.
    fn applied_reply(&self, actor_key: &str, idem_key: &str) -> Option<EntityResponse> {
        self.with_entry(actor_key, idem_key, |entry| {
            entry.effects_applied.then(|| entry.response.clone())
        })
        .flatten()
    }

    /// Read the live (unexpired) entry for `idem_key`.
    fn with_entry<T>(
        &self,
        actor_key: &str,
        idem_key: &str,
        read: impl FnOnce(&IdempotencyEntry) -> T,
    ) -> Option<T> {
        let now = sim_now();
        let entries = match self.entries.read() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = entries.by_actor.get(actor_key)?.get(idem_key)?;
        (!is_expired(entry, now)).then(|| read(entry))
    }

    /// Cache a response for a given actor and idempotency key.
    ///
    /// Keeps a copy without event parameters or the durable key set. When
    /// the cache is over budget, replies are evicted (see the module docs).
    pub fn put(
        &self,
        actor_key: &str,
        idem_key: &str,
        request: &RequestFingerprint,
        response: &EntityResponse,
    ) {
        self.put_with_effects_applied(actor_key, idem_key, request, response, false);
    }

    /// Cache a response whose post-dispatch effects are known to be complete.
    pub fn put_effects_applied(
        &self,
        actor_key: &str,
        idem_key: &str,
        request: &RequestFingerprint,
        response: &EntityResponse,
    ) {
        self.put_with_effects_applied(actor_key, idem_key, request, response, true);
    }

    fn put_with_effects_applied(
        &self,
        actor_key: &str,
        idem_key: &str,
        request: &RequestFingerprint,
        response: &EntityResponse,
        effects_applied: bool,
    ) {
        let response = replay_copy(response);
        let bytes = estimated_entry_bytes(actor_key, idem_key, &response);

        let now = sim_now();
        let mut entries = self.entries.write().unwrap(); // ci-ok: infallible lock
        entries.remove(actor_key, idem_key);
        entries.evict_expired(now);

        // A reply larger than the whole budget is not cached. The durable key
        // set still deduplicates its retry, but as after a restart, a retry
        // of a dropped reply then runs without the reply's spawns, timers and
        // (for a non-composite action) triggers.
        if bytes > self.budget_bytes {
            return;
        }

        // Check the two inserts locally. Recounting every actor's replies here
        // makes each debug-build write linear in the live actor count. Tests
        // check the whole-cache indexes across replacement, eviction and expiry.
        let replies_before_insert = entries.len();
        let seq = entries.next_seq;
        entries.next_seq += 1;
        let order = if effects_applied {
            &mut entries.applied
        } else {
            &mut entries.pending
        };
        order.insert(seq, (actor_key.to_string(), idem_key.to_string()));
        let inserted_new_reply = entries
            .by_actor
            .entry(actor_key.to_string())
            .or_default()
            .insert(
                idem_key.to_string(),
                IdempotencyEntry {
                    request: request.clone(),
                    response,
                    created_at: now,
                    effects_applied,
                    bytes,
                    seq,
                },
            )
            .is_none();
        debug_assert!(inserted_new_reply);
        debug_assert_eq!(entries.len(), replies_before_insert + 1);
        entries.bytes += bytes;

        while entries.bytes > self.budget_bytes {
            if !entries.evict_one() {
                break;
            }
        }
        debug_assert!(entries.bytes <= self.budget_bytes);
    }
}

impl Default for IdempotencyCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the actor's live state is still exactly the state a cached reply
/// was taken from, so a retry can be answered with it in full. The sequence
/// number alone cannot tell: it stays 0 without a journal, and a state
/// restored from a snapshot keeps it with fewer recent events. So the newest
/// live event must be this retry's, the event counts must match, and the live
/// history must be at least as long as the cached one.
pub(crate) fn live_state_is_the_reply(live: &EntityState, cached: &EntityState, key: &str) -> bool {
    live.events
        .back()
        .and_then(|event| event.idempotency_key.as_deref())
        == Some(key)
        && live.total_event_count == cached.total_event_count
        && live.events.len() >= cached.events.len()
}

#[cfg(test)]
mod tests;
