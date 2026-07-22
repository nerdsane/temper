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
    response: Option<EntityResponse>,
    /// When this entry was created (for TTL eviction).
    created_at: chrono::DateTime<chrono::Utc>,
    /// Whether dispatcher-side post-dispatch effects have completed for this
    /// cached response.
    effects_applied: bool,
    /// Canonical action + params + principal proof for safe post-action replay.
    bound_action_fingerprint: Option<String>,
    /// Original action parameters paired with `bound_action_fingerprint`.
    bound_action_params: Option<serde_json::Value>,
    /// Whether the bound-action post-action hook completed successfully.
    bound_action_hook_completed: bool,
    /// Whether one request currently owns execution of the post-action hook.
    bound_action_hook_in_flight: bool,
    /// Exact hook output merged into the successful protocol response.
    bound_action_hook_output: Option<serde_json::Value>,
    /// Keep an exact post-action replay proof while an outcome-ambiguous
    /// publication debt is armed. The sticky tenant gate can outlive the normal
    /// cache TTL, and evicting its only memory-mode recovery proof would make
    /// the gate impossible to discharge.
    publication_replay_pinned: bool,
}

/// Result of looking up an effects-applied bound action for post-action replay.
#[derive(Debug, Clone)]
pub enum BoundActionReplayLookup {
    /// No live, effects-applied bound-action proof exists for this key.
    Miss,
    /// The retry exactly matches the originally authorized action request.
    Match {
        /// Cached entity response produced by the governed action.
        response: Box<EntityResponse>,
        /// Original parameters supplied to the post-action hook.
        params: serde_json::Value,
        /// Whether the post-action hook has already completed.
        hook_completed: bool,
        /// Cached post-action output from the completed hook.
        hook_output: Option<serde_json::Value>,
    },
    /// The exact replay proof exists, but another request owns its hook.
    Pending,
    /// The idempotency key exists but action, parameters, or principal differ.
    Conflict,
}

/// Atomic result of reserving a raw bound-action idempotency key.
#[derive(Debug, Clone)]
pub enum BoundActionClaim {
    /// This request owns the new reservation and may dispatch the action.
    Claimed,
    /// The exact same request already owns an unfinished reservation.
    Pending,
    /// The exact request has a completed actor response whose hook may replay.
    Match {
        /// Cached entity response produced by the governed action.
        response: Box<EntityResponse>,
        /// Original parameters supplied to the post-action hook.
        params: serde_json::Value,
        /// Whether the post-action hook has already completed.
        hook_completed: bool,
        /// Cached post-action output from the completed hook.
        hook_output: Option<serde_json::Value>,
    },
    /// The raw key is already owned by another request or by an unproved legacy entry.
    Conflict,
    /// Every actor-local cache slot is protected by unfinished work.
    AtCapacity,
}

/// Per-entity-actor idempotency cache.
///
/// Thread-safe via `RwLock`. Uses `BTreeMap` for deterministic iteration
/// order (DST compliance).
pub struct IdempotencyCache {
    /// actor_key → (idempotency_key → entry).
    entries: RwLock<BTreeMap<String, BTreeMap<String, IdempotencyEntry>>>,
}

/// Cancellation guard for one newly claimed raw bound-action key.
#[must_use = "dropping the guard releases the raw bound-action reservation"]
pub(crate) struct BoundActionReservationGuard<'a> {
    cache: &'a IdempotencyCache,
    actor_key: String,
    idem_key: String,
    request_fingerprint: String,
    armed: bool,
}

impl BoundActionReservationGuard<'_> {
    /// Disarm after the raw reservation has been atomically replaced by its
    /// completed actor response.
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for BoundActionReservationGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.cache.abandon_bound_action_claim(
                &self.actor_key,
                &self.idem_key,
                &self.request_fingerprint,
            );
        }
    }
}

/// Cancellation guard for one owned post-action hook replay.
#[must_use = "dropping the guard releases the post-action hook claim"]
pub(crate) struct BoundActionHookGuard<'a, F>
where
    F: Fn() -> bool,
{
    cache: &'a IdempotencyCache,
    actor_key: String,
    idem_key: String,
    request_fingerprint: String,
    publication_gated: F,
    armed: bool,
}

impl<F> BoundActionHookGuard<'_, F>
where
    F: Fn() -> bool,
{
    /// Disarm after durable receipt persistence and cache completion succeed.
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl<F> Drop for BoundActionHookGuard<'_, F>
where
    F: Fn() -> bool,
{
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let publication_gated = (self.publication_gated)();
        self.cache.release_bound_action_hook(
            &self.actor_key,
            &self.idem_key,
            &self.request_fingerprint,
            publication_gated,
        );
    }
}

impl IdempotencyCache {
    /// Create a new empty idempotency cache.
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
        }
    }

    /// Arm cancellation cleanup for a raw bound-action reservation owned by
    /// the current request.
    pub(crate) fn guard_bound_action_reservation(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
    ) -> BoundActionReservationGuard<'_> {
        BoundActionReservationGuard {
            cache: self,
            actor_key: actor_key.to_string(),
            idem_key: idem_key.to_string(),
            request_fingerprint: request_fingerprint.to_string(),
            armed: true,
        }
    }

    /// Arm cancellation cleanup for an owned hook replay. The supplied gate
    /// check is evaluated at drop time so a hook that arms publication debt
    /// before cancellation retains its exact recovery proof.
    pub(crate) fn guard_bound_action_hook<F>(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
        publication_gated: F,
    ) -> BoundActionHookGuard<'_, F>
    where
        F: Fn() -> bool,
    {
        BoundActionHookGuard {
            cache: self,
            actor_key: actor_key.to_string(),
            idem_key: idem_key.to_string(),
            request_fingerprint: request_fingerprint.to_string(),
            publication_gated,
            armed: true,
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

        entry.response.clone()
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
        let entries = self
            .entries
            .read()
            .expect("idempotency cache lock poisoned");
        let actor_entries = entries.get(actor_key)?;
        let entry = actor_entries.get(idem_key)?;

        let age = now.signed_duration_since(entry.created_at);
        if age.num_seconds() > IDEMPOTENCY_TTL_SECS || !entry.effects_applied {
            return None;
        }

        entry.response.clone()
    }

    /// Cache a response for a given actor and idempotency key.
    ///
    /// If the per-actor budget is exceeded, the oldest entry is evicted.
    pub fn put(&self, actor_key: &str, idem_key: &str, response: EntityResponse) {
        let _ = self.put_with_effects_applied(actor_key, idem_key, response, false, None);
    }

    /// Cache a response whose post-dispatch effects are known to be complete.
    pub fn put_effects_applied(&self, actor_key: &str, idem_key: &str, response: EntityResponse) {
        let _ = self.put_with_effects_applied(actor_key, idem_key, response, true, None);
    }

    /// Cache an effects-applied bound action together with the exact request
    /// proof required to replay its post-action hook safely.
    pub fn put_bound_action_effects_applied(
        &self,
        actor_key: &str,
        idem_key: &str,
        response: EntityResponse,
        request_fingerprint: String,
        params: serde_json::Value,
    ) -> bool {
        self.put_with_effects_applied(
            actor_key,
            idem_key,
            response,
            true,
            Some((request_fingerprint, params)),
        )
    }

    /// Look up a post-action replay and reject reuse of an idempotency key by a
    /// different action request or principal.
    pub fn lookup_bound_action_replay(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
    ) -> BoundActionReplayLookup {
        let now = sim_now();
        let mut entries = self
            .entries
            .write()
            .expect("idempotency cache lock poisoned");
        let Some(entry) = entries
            .get_mut(actor_key)
            .and_then(|actor_entries| actor_entries.get_mut(idem_key))
        else {
            return BoundActionReplayLookup::Miss;
        };
        let age = now.signed_duration_since(entry.created_at);
        if age.num_seconds() > IDEMPOTENCY_TTL_SECS
            && !entry.publication_replay_pinned
            && !entry.bound_action_hook_in_flight
        {
            return BoundActionReplayLookup::Miss;
        }
        match (
            entry.bound_action_fingerprint.as_deref(),
            entry.bound_action_params.as_ref(),
        ) {
            (Some(stored), Some(params)) if stored == request_fingerprint => {
                match entry.response.as_ref() {
                    Some(response)
                        if entry.effects_applied && entry.bound_action_hook_completed =>
                    {
                        BoundActionReplayLookup::Match {
                            response: Box::new(response.clone()),
                            params: params.clone(),
                            hook_completed: true,
                            hook_output: entry.bound_action_hook_output.clone(),
                        }
                    }
                    Some(_) if entry.effects_applied && entry.bound_action_hook_in_flight => {
                        BoundActionReplayLookup::Pending
                    }
                    Some(response) if entry.effects_applied => {
                        entry.bound_action_hook_in_flight = true;
                        BoundActionReplayLookup::Match {
                            response: Box::new(response.clone()),
                            params: params.clone(),
                            hook_completed: false,
                            hook_output: None,
                        }
                    }
                    _ => BoundActionReplayLookup::Miss,
                }
            }
            (Some(_), Some(_)) => BoundActionReplayLookup::Conflict,
            _ => BoundActionReplayLookup::Conflict,
        }
    }

    /// Atomically reserve a raw idempotency key for one exact bound-action
    /// request. This closes the interval between protocol lookup and actor
    /// completion in which another action could otherwise reuse the key.
    pub fn claim_bound_action(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
        params: &serde_json::Value,
    ) -> BoundActionClaim {
        let now = sim_now();
        let mut entries = self
            .entries
            .write()
            .expect("idempotency cache lock poisoned");
        let actor_entries = entries.entry(actor_key.to_string()).or_default();
        actor_entries.retain(|_, entry| {
            Self::entry_is_protected(entry)
                || now.signed_duration_since(entry.created_at).num_seconds() <= IDEMPOTENCY_TTL_SECS
        });

        if let Some(entry) = actor_entries.get_mut(idem_key) {
            return match (
                entry.bound_action_fingerprint.as_deref(),
                entry.bound_action_params.as_ref(),
            ) {
                (Some(stored), Some(stored_params)) if stored == request_fingerprint => {
                    match entry.response.as_ref() {
                        Some(response)
                            if entry.effects_applied && entry.bound_action_hook_completed =>
                        {
                            BoundActionClaim::Match {
                                response: Box::new(response.clone()),
                                params: stored_params.clone(),
                                hook_completed: true,
                                hook_output: entry.bound_action_hook_output.clone(),
                            }
                        }
                        Some(_) if entry.effects_applied && entry.bound_action_hook_in_flight => {
                            BoundActionClaim::Pending
                        }
                        Some(response) if entry.effects_applied => {
                            entry.bound_action_hook_in_flight = true;
                            BoundActionClaim::Match {
                                response: Box::new(response.clone()),
                                params: stored_params.clone(),
                                hook_completed: false,
                                hook_output: None,
                            }
                        }
                        _ => BoundActionClaim::Pending,
                    }
                }
                _ => BoundActionClaim::Conflict,
            };
        }

        if !Self::make_room_for_new_entry(actor_entries) {
            return BoundActionClaim::AtCapacity;
        }
        actor_entries.insert(
            idem_key.to_string(),
            IdempotencyEntry {
                response: None,
                created_at: now,
                effects_applied: false,
                bound_action_fingerprint: Some(request_fingerprint.to_string()),
                bound_action_params: Some(params.clone()),
                bound_action_hook_completed: false,
                bound_action_hook_in_flight: false,
                bound_action_hook_output: None,
                publication_replay_pinned: false,
            },
        );
        BoundActionClaim::Claimed
    }

    /// Mark one exact bound-action hook successful and retain its response
    /// output so later protocol retries do not repeat external work.
    pub fn complete_bound_action_hook(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
        hook_output: Option<serde_json::Value>,
    ) -> bool {
        let mut entries = self
            .entries
            .write()
            .expect("idempotency cache lock poisoned");
        let Some(entry) = entries
            .get_mut(actor_key)
            .and_then(|actor_entries| actor_entries.get_mut(idem_key))
        else {
            return false;
        };
        if !entry.effects_applied
            || entry.response.is_none()
            || entry.bound_action_fingerprint.as_deref() != Some(request_fingerprint)
        {
            return false;
        }
        entry.bound_action_hook_completed = true;
        entry.bound_action_hook_in_flight = false;
        entry.bound_action_hook_output = hook_output;
        entry.publication_replay_pinned = false;
        entry.created_at = sim_now();
        true
    }

    /// Release one failed hook claim so an exact retry can own it.
    pub fn fail_bound_action_hook(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
    ) {
        self.release_bound_action_hook(actor_key, idem_key, request_fingerprint, false);
    }

    fn release_bound_action_hook(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
        pin_publication_replay: bool,
    ) {
        let mut entries = self
            .entries
            .write()
            .expect("idempotency cache lock poisoned");
        if let Some(entry) = entries
            .get_mut(actor_key)
            .and_then(|actor_entries| actor_entries.get_mut(idem_key))
            && entry.effects_applied
            && !entry.bound_action_hook_completed
            && entry.bound_action_hook_in_flight
            && entry.bound_action_fingerprint.as_deref() == Some(request_fingerprint)
        {
            entry.bound_action_hook_in_flight = false;
            entry.publication_replay_pinned |= pin_publication_replay;
            entry.created_at = sim_now();
        }
    }

    /// Pin the exact completed replay proof responsible for an armed
    /// publication debt until its retry succeeds.
    pub fn pin_bound_action_replay(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
    ) -> bool {
        let mut entries = self
            .entries
            .write()
            .expect("idempotency cache lock poisoned");
        let Some(entry) = entries
            .get_mut(actor_key)
            .and_then(|actor_entries| actor_entries.get_mut(idem_key))
        else {
            return false;
        };
        if entry.effects_applied
            && entry.response.is_some()
            && entry.bound_action_fingerprint.as_deref() == Some(request_fingerprint)
        {
            entry.publication_replay_pinned = true;
            return true;
        }
        false
    }

    /// Return a completed publication replay to ordinary bounded-cache policy
    /// after its sticky debt has been discharged. Incomplete or in-flight hook
    /// claims retain the pin because cancellation can still require recovery.
    pub fn unpin_bound_action_replay(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
    ) {
        let mut entries = self
            .entries
            .write()
            .expect("idempotency cache lock poisoned");
        if let Some(entry) = entries
            .get_mut(actor_key)
            .and_then(|actor_entries| actor_entries.get_mut(idem_key))
            && entry.bound_action_fingerprint.as_deref() == Some(request_fingerprint)
            && entry.bound_action_hook_completed
            && !entry.bound_action_hook_in_flight
        {
            entry.publication_replay_pinned = false;
            entry.created_at = sim_now();
        }
    }

    /// Release an unfinished reservation after dispatch fails without a
    /// durable action response. A newer owner or completed response is retained.
    pub fn abandon_bound_action_claim(
        &self,
        actor_key: &str,
        idem_key: &str,
        request_fingerprint: &str,
    ) {
        let mut entries = self
            .entries
            .write()
            .expect("idempotency cache lock poisoned");
        let Some(actor_entries) = entries.get_mut(actor_key) else {
            return;
        };
        let remove = actor_entries.get(idem_key).is_some_and(|entry| {
            entry.response.is_none()
                && entry.bound_action_fingerprint.as_deref() == Some(request_fingerprint)
        });
        if remove {
            actor_entries.remove(idem_key);
        }
    }

    #[cfg(test)]
    pub(crate) fn clear_actor_for_test(&self, actor_key: &str) {
        self.entries.write().unwrap().remove(actor_key); // ci-ok: test-only lock
    }

    /// Whether this key names a live, effects-applied bound action. Admission
    /// uses this only to reach the handler, which still verifies the fingerprint.
    pub fn has_bound_action_replay(&self, actor_key: &str, idem_key: &str) -> bool {
        let now = sim_now();
        let entries = self
            .entries
            .read()
            .expect("idempotency cache lock poisoned");
        entries
            .get(actor_key)
            .and_then(|actor_entries| actor_entries.get(idem_key))
            .is_some_and(|entry| {
                (entry.publication_replay_pinned
                    || now.signed_duration_since(entry.created_at).num_seconds()
                        <= IDEMPOTENCY_TTL_SECS)
                    && entry.effects_applied
                    && entry.response.is_some()
                    && entry.bound_action_fingerprint.is_some()
                    && entry.bound_action_params.is_some()
            })
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
        bound_action: Option<(String, serde_json::Value)>,
    ) -> bool {
        let now = sim_now();
        let mut entries = self
            .entries
            .write()
            .expect("idempotency cache lock poisoned");
        let actor_entries = entries.entry(actor_key.to_string()).or_default();

        // Evict expired entries first.
        actor_entries.retain(|_, entry| {
            Self::entry_is_protected(entry)
                || now.signed_duration_since(entry.created_at).num_seconds() <= IDEMPOTENCY_TTL_SECS
        });

        if !actor_entries.contains_key(idem_key) && !Self::make_room_for_new_entry(actor_entries) {
            return false;
        }

        let inherited_bound_action = actor_entries.get(idem_key).and_then(|entry| {
            entry
                .bound_action_fingerprint
                .clone()
                .zip(entry.bound_action_params.clone())
        });
        let inherited_pin = actor_entries
            .get(idem_key)
            .is_some_and(|entry| entry.publication_replay_pinned);
        let inherited_hook = actor_entries.get(idem_key).map(|entry| {
            (
                entry.bound_action_hook_completed,
                entry.bound_action_hook_in_flight,
                entry.bound_action_hook_output.clone(),
            )
        });
        let resets_bound_action_hook = bound_action.is_some();
        let bound_action = bound_action.or(inherited_bound_action);
        let (bound_action_hook_completed, bound_action_hook_in_flight, bound_action_hook_output) =
            if resets_bound_action_hook {
                (false, true, None)
            } else {
                inherited_hook.unwrap_or((false, false, None))
            };
        actor_entries.insert(
            idem_key.to_string(),
            IdempotencyEntry {
                response: Some(response),
                created_at: now,
                effects_applied,
                bound_action_fingerprint: bound_action
                    .as_ref()
                    .map(|(fingerprint, _)| fingerprint.clone()),
                bound_action_params: bound_action.map(|(_, params)| params),
                bound_action_hook_completed,
                bound_action_hook_in_flight,
                bound_action_hook_output,
                publication_replay_pinned: inherited_pin,
            },
        );
        true
    }

    fn make_room_for_new_entry(actor_entries: &mut BTreeMap<String, IdempotencyEntry>) -> bool {
        // Budget enforcement: evict oldest replaceable entries until a new
        // admission fits. Active reservations are protected because evicting
        // one would allow the same raw key to be claimed concurrently.
        while actor_entries.len() >= IDEMPOTENCY_BUDGET_PER_ACTOR {
            if let Some(oldest_key) = actor_entries
                .iter()
                .filter(|(_, entry)| !Self::entry_is_protected(entry))
                .min_by_key(|(_, e)| e.created_at)
                .map(|(k, _)| k.clone())
            {
                actor_entries.remove(&oldest_key);
            } else {
                return false;
            }
        }
        debug_assert!(actor_entries.len() < IDEMPOTENCY_BUDGET_PER_ACTOR);
        true
    }

    fn entry_is_protected(entry: &IdempotencyEntry) -> bool {
        entry.publication_replay_pinned
            || entry.bound_action_hook_in_flight
            || (entry.response.is_none() && entry.bound_action_fingerprint.is_some())
    }
}

impl Default for IdempotencyCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "idempotency_test.rs"]
mod tests;
