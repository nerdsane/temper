//! Idempotency cache for deduplicating agent retries.
//!
//! Per-entity-actor LRU cache of recent `Idempotency-Key` → `EntityResponse`.
//! Entries expire after `IDEMPOTENCY_TTL_SECS` and are evicted when the
//! per-actor budget is exceeded.

use std::collections::BTreeMap;
use std::sync::RwLock;

use temper_runtime::scheduler::sim_now;

use crate::entity_actor::EntityResponse;

/// Maximum number of idempotency entries per actor (TigerStyle budget).
pub const IDEMPOTENCY_BUDGET_PER_ACTOR: usize = 1_000;

/// Time-to-live for idempotency entries in seconds.
pub const IDEMPOTENCY_TTL_SECS: i64 = 3600;

/// A cached idempotent response.
struct IdempotencyEntry {
    /// The cached response to return on duplicate requests.
    response: EntityResponse,
    /// When this entry was created (for TTL eviction).
    created_at: chrono::DateTime<chrono::Utc>,
    /// Whether dispatcher-side post-dispatch effects have completed for this
    /// cached response.
    effects_applied: bool,
}

/// Per-entity-actor idempotency cache.
///
/// Thread-safe via `RwLock`. Uses `BTreeMap` for deterministic iteration
/// order (DST compliance).
pub struct IdempotencyCache {
    /// actor_key → (idempotency_key → entry).
    entries: RwLock<BTreeMap<String, BTreeMap<String, IdempotencyEntry>>>,
}

impl IdempotencyCache {
    /// Create a new empty idempotency cache.
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
        }
    }

    /// Look up a cached response. Returns `None` if not found or expired.
    pub fn get(&self, actor_key: &str, idem_key: &str) -> Option<EntityResponse> {
        let now = sim_now();
        let entries = match self.entries.read() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let actor_entries = entries.get(actor_key)?;
        let entry = actor_entries.get(idem_key)?;

        // Check TTL
        let age = now.signed_duration_since(entry.created_at);
        if age.num_seconds() > IDEMPOTENCY_TTL_SECS {
            return None;
        }

        Some(entry.response.clone())
    }

    /// Look up a cached response only after post-dispatch effects have run.
    ///
    /// HTTP/OData callers use this stricter lookup so a retry after a dropped
    /// actor reply re-enters dispatch and fires effects instead of short-
    /// circuiting at the protocol boundary.
    pub fn get_after_effects_applied(
        &self,
        actor_key: &str,
        idem_key: &str,
    ) -> Option<EntityResponse> {
        let now = sim_now();
        let entries = self.entries.read().unwrap(); // ci-ok: infallible lock
        let actor_entries = entries.get(actor_key)?;
        let entry = actor_entries.get(idem_key)?;

        let age = now.signed_duration_since(entry.created_at);
        if age.num_seconds() > IDEMPOTENCY_TTL_SECS || !entry.effects_applied {
            return None;
        }

        Some(entry.response.clone())
    }

    /// Cache a response for a given actor and idempotency key.
    ///
    /// If the per-actor budget is exceeded, the oldest entry is evicted.
    pub fn put(&self, actor_key: &str, idem_key: &str, response: EntityResponse) {
        self.put_with_effects_applied(actor_key, idem_key, response, false);
    }

    /// Cache a response whose post-dispatch effects are known to be complete.
    pub fn put_effects_applied(&self, actor_key: &str, idem_key: &str, response: EntityResponse) {
        self.put_with_effects_applied(actor_key, idem_key, response, true);
    }

    /// Mark a cached response as having completed post-dispatch effects.
    pub fn mark_effects_applied(&self, actor_key: &str, idem_key: &str) -> bool {
        let now = sim_now();
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(actor_entries) = entries.get_mut(actor_key) else {
            return false;
        };
        let Some(entry) = actor_entries.get_mut(idem_key) else {
            return false;
        };

        let age = now.signed_duration_since(entry.created_at);
        if age.num_seconds() > IDEMPOTENCY_TTL_SECS {
            return false;
        }

        entry.effects_applied = true;
        true
    }

    fn put_with_effects_applied(
        &self,
        actor_key: &str,
        idem_key: &str,
        response: EntityResponse,
        effects_applied: bool,
    ) {
        let now = sim_now();
        let mut entries = self.entries.write().unwrap(); // ci-ok: infallible lock
        let actor_entries = entries.entry(actor_key.to_string()).or_default();

        // Evict expired entries first.
        actor_entries.retain(|_, entry| {
            now.signed_duration_since(entry.created_at).num_seconds() <= IDEMPOTENCY_TTL_SECS
        });

        // Budget enforcement: evict oldest if at capacity.
        while actor_entries.len() >= IDEMPOTENCY_BUDGET_PER_ACTOR {
            // Find the oldest entry by created_at.
            if let Some(oldest_key) = actor_entries
                .iter()
                .min_by_key(|(_, e)| e.created_at)
                .map(|(k, _)| k.clone())
            {
                actor_entries.remove(&oldest_key);
            } else {
                break;
            }
        }

        actor_entries.insert(
            idem_key.to_string(),
            IdempotencyEntry {
                response,
                created_at: now,
                effects_applied,
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
mod tests {
    use super::*;
    use crate::entity_actor::{EntityResponse, EntityState};
    use std::collections::BTreeMap;

    fn make_response(status: &str) -> EntityResponse {
        EntityResponse {
            success: true,
            state: EntityState {
                entity_type: String::new(),
                entity_id: String::new(),
                status: status.to_string(),
                item_count: 0,
                counters: BTreeMap::new(),
                booleans: BTreeMap::new(),
                lists: BTreeMap::new(),
                fields: serde_json::json!({}),
                events: std::collections::VecDeque::new(),
                total_event_count: 0,
                events_since_snapshot: 0,
                last_snapshot_sequence_nr: 0,
                sequence_nr: 0,
                processed_idempotency_keys: BTreeMap::new(),
            },
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        }
    }

    #[test]
    fn put_then_get_returns_cached() {
        let cache = IdempotencyCache::new();
        let resp = make_response("Active");
        cache.put("Order:o1", "key-1", resp.clone());
        let cached = cache.get("Order:o1", "key-1");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().state.status, "Active");
    }

    #[test]
    fn pending_effects_do_not_satisfy_protocol_cache_hit() {
        let cache = IdempotencyCache::new();
        cache.put("Order:o1", "key-1", make_response("Active"));

        assert!(cache.get("Order:o1", "key-1").is_some());
        assert!(
            cache
                .get_after_effects_applied("Order:o1", "key-1")
                .is_none()
        );

        assert!(cache.mark_effects_applied("Order:o1", "key-1"));
        assert!(
            cache
                .get_after_effects_applied("Order:o1", "key-1")
                .is_some()
        );
    }

    #[test]
    fn put_effects_applied_satisfies_protocol_cache_hit() {
        let cache = IdempotencyCache::new();
        cache.put_effects_applied("Order:o1", "key-1", make_response("Active"));

        let cached = cache.get_after_effects_applied("Order:o1", "key-1");
        assert_eq!(cached.unwrap().state.status, "Active");
    }

    #[test]
    fn get_missing_returns_none() {
        let cache = IdempotencyCache::new();
        assert!(cache.get("Order:o1", "no-such-key").is_none());
    }

    #[test]
    fn different_actors_isolated() {
        let cache = IdempotencyCache::new();
        cache.put("Order:o1", "key-1", make_response("A"));
        cache.put("Order:o2", "key-1", make_response("B"));
        assert_eq!(cache.get("Order:o1", "key-1").unwrap().state.status, "A");
        assert_eq!(cache.get("Order:o2", "key-1").unwrap().state.status, "B");
    }

    #[test]
    fn budget_evicts_oldest() {
        let cache = IdempotencyCache::new();
        // Fill to budget
        for i in 0..IDEMPOTENCY_BUDGET_PER_ACTOR {
            cache.put("actor", &format!("k-{i}"), make_response("S"));
        }
        // One more should evict the oldest
        cache.put("actor", "k-overflow", make_response("New"));
        let entries = cache.entries.read().unwrap();
        let actor_entries = entries.get("actor").unwrap();
        assert_eq!(actor_entries.len(), IDEMPOTENCY_BUDGET_PER_ACTOR);
        assert!(actor_entries.contains_key("k-overflow"));
    }
}
