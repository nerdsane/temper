//! Monotonic ownership and cancellation state for declared timeouts.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::StateTimeout;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct EntityKey {
    tenant: String,
    entity_type: String,
    entity_id: String,
}

impl EntityKey {
    pub(super) fn new(tenant: &TenantId, entity_type: &str, entity_id: &str) -> Self {
        Self {
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        }
    }
}

/// In-memory cancellation counter keyed by entity instance.
///
/// Each accepted committed response increments and captures a generation;
/// firings compare the captured generation against the current owner and drop
/// the fire when they diverge. Journal sequence numbers make persisted response
/// acceptance monotonic; in-memory actors use their total applied event count.
#[derive(Default, Debug)]
pub struct StateTimeoutTracker {
    owners: Mutex<BTreeMap<EntityKey, StateTimeoutOwner>>,
    next_generation: Mutex<u64>,
    hydration_readiness:
        Mutex<BTreeMap<(EntityKey, uuid::Uuid), tokio::sync::watch::Receiver<bool>>>,
    /// ADR-0049: per-entity-type count of armed-but-unfired timers.
    /// Emitted as `temper_scheduler_pending_timers` by the canary loop.
    pending_by_type: Mutex<BTreeMap<String, u64>>,
    /// Single lifecycle owner for the registry-wide timeout reconciler.
    registry_reconciliation_lifetime: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    #[cfg(test)]
    reconciliation_failures: Mutex<u64>,
    #[cfg(test)]
    registry_scan_gate: Mutex<Option<RegistryScanGate>>,
}

#[cfg(test)]
#[derive(Debug)]
struct RegistryScanGate {
    captured: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

#[derive(Debug, Default)]
struct StateTimeoutOwner {
    generation: u64,
    event_order: u64,
    reset_at: Option<DateTime<Utc>>,
    reset_version: Option<u64>,
    declaration: Option<StateTimeout>,
    cancellation: Option<tokio::sync::watch::Sender<bool>>,
}

pub(super) struct StateTimeoutPermit {
    pub(super) generation: u64,
    pub(super) cancellation: tokio::sync::watch::Receiver<bool>,
}

/// Exact inactive owner established by one synthetic durable commit.
///
/// Cleanup must present this provenance so an older eviction cannot remove a
/// fence advanced by a replacement incarnation or a later commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InactiveStateTimeoutFence {
    pub(super) generation: u64,
    pub(super) event_order: u64,
}

impl StateTimeoutTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the single registry timeout reconciler and return its shutdown signal.
    pub(crate) fn begin_registry_reconciliation(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        let mut lifetime = self
            .registry_reconciliation_lifetime
            .lock()
            .expect("registry reconciliation lifetime poisoned");
        if lifetime.is_some() {
            return None;
        }
        let (sender, receiver) = tokio::sync::watch::channel(false);
        *lifetime = Some(sender);
        Some(receiver)
    }

    /// Release the registry timeout reconciler claim after its task exits.
    pub(crate) fn finish_registry_reconciliation(&self) {
        let sender = self
            .registry_reconciliation_lifetime
            .lock()
            .expect("registry reconciliation lifetime poisoned")
            .take();
        if let Some(sender) = sender {
            sender.send_replace(true);
        }
    }

    /// Register the first-mailbox hydration barrier before an actor is visible.
    pub(crate) fn register_hydration(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        actor_uid: uuid::Uuid,
    ) -> tokio::sync::watch::Sender<bool> {
        let key = EntityKey::new(tenant, entity_type, entity_id);
        let (completion, readiness) = tokio::sync::watch::channel(false);
        self.hydration_readiness
            .lock()
            .expect("state_timeout hydration readiness poisoned")
            .insert((key, actor_uid), readiness);
        completion
    }

    /// Wait until the first-mailbox response has reconciled timeout ownership.
    pub(crate) async fn wait_for_hydration(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        actor_uid: uuid::Uuid,
    ) {
        let key = EntityKey::new(tenant, entity_type, entity_id);
        let readiness = self
            .hydration_readiness
            .lock()
            .expect("state_timeout hydration readiness poisoned")
            .get(&(key, actor_uid))
            .cloned();
        let Some(mut readiness) = readiness else {
            return;
        };
        if !*readiness.borrow() {
            let _ = readiness.changed().await;
        }
    }

    /// Publish completion and reclaim the exact incarnation's readiness row.
    pub(crate) fn complete_hydration(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        actor_uid: uuid::Uuid,
        completion: tokio::sync::watch::Sender<bool>,
    ) {
        completion.send_replace(true);
        let key = EntityKey::new(tenant, entity_type, entity_id);
        self.hydration_readiness
            .lock()
            .expect("state_timeout hydration readiness poisoned")
            .remove(&(key, actor_uid));
    }

    fn next_generation(&self) -> u64 {
        let mut next = self
            .next_generation
            .lock()
            .expect("state_timeout generation poisoned");
        *next = next
            .checked_add(1)
            .expect("state timeout generation overflow");
        *next
    }

    fn cancel_task(owner: &mut StateTimeoutOwner) {
        if let Some(cancellation) = owner.cancellation.take() {
            cancellation.send_replace(true);
        }
    }

    fn replace_task(owner: &mut StateTimeoutOwner, generation: u64) -> StateTimeoutPermit {
        Self::cancel_task(owner);
        let (cancellation, receiver) = tokio::sync::watch::channel(false);
        owner.generation = generation;
        owner.cancellation = Some(cancellation);
        StateTimeoutPermit {
            generation,
            cancellation: receiver,
        }
    }

    /// Advance timeout ownership for a strictly newer committed response.
    pub(super) fn advance_if_fresh(
        &self,
        key: &EntityKey,
        event_order: u64,
        reset_at: Option<DateTime<Utc>>,
        reset_version: Option<u64>,
        declaration: Option<&StateTimeout>,
    ) -> Option<StateTimeoutPermit> {
        let mut map = self.owners.lock().expect("state_timeout tracker poisoned");
        let owner = map.entry(key.clone()).or_default();
        let declaration_changed = owner.declaration.as_ref() != declaration;
        if owner.generation != 0
            && (event_order < owner.event_order
                || (event_order == owner.event_order && !declaration_changed))
        {
            return None;
        }
        let generation = self.next_generation();
        let permit = Self::replace_task(owner, generation);
        owner.event_order = event_order;
        owner.reset_at = reset_at;
        owner.reset_version = reset_version;
        owner.declaration = declaration.cloned();
        Some(permit)
    }

    /// Observe a newer response and arm only when ownership is missing or its
    /// durable clock anchor changed.
    pub(super) fn reconcile_if_fresh(
        &self,
        key: &EntityKey,
        event_order: u64,
        reset_at: Option<DateTime<Utc>>,
        reset_version: Option<u64>,
        declaration: Option<&StateTimeout>,
    ) -> Option<StateTimeoutPermit> {
        let mut map = self.owners.lock().expect("state_timeout tracker poisoned");
        let owner = map.entry(key.clone()).or_default();
        if owner.generation != 0 && event_order < owner.event_order {
            return None;
        }
        let needs_arm = owner.generation == 0
            || owner.reset_at != reset_at
            || owner.reset_version != reset_version
            || owner.declaration.as_ref() != declaration;
        owner.event_order = event_order;
        if !needs_arm {
            return None;
        }
        let generation = self.next_generation();
        let permit = Self::replace_task(owner, generation);
        owner.reset_at = reset_at;
        owner.reset_version = reset_version;
        owner.declaration = declaration.cloned();
        Some(permit)
    }

    /// Invalidate a declaration removed from the current table without
    /// requiring another domain event.
    pub(super) fn invalidate_if_fresh(
        &self,
        key: &EntityKey,
        event_order: u64,
        reset_at: Option<DateTime<Utc>>,
        reset_version: Option<u64>,
    ) -> bool {
        let mut map = self.owners.lock().expect("state_timeout tracker poisoned");
        let owner = map.entry(key.clone()).or_default();
        if owner.generation != 0 {
            if event_order < owner.event_order {
                return false;
            }
            if event_order == owner.event_order && owner.declaration.is_none() {
                return false;
            }
        }
        Self::cancel_task(owner);
        owner.generation = self.next_generation();
        owner.event_order = event_order;
        owner.reset_at = reset_at;
        owner.reset_version = reset_version;
        owner.declaration = None;
        true
    }

    /// Establish a monotonic inactive fence, including when no owner existed.
    pub(super) fn fence_inactive_if_fresh(
        &self,
        key: &EntityKey,
        event_order: u64,
        reset_at: Option<DateTime<Utc>>,
        reset_version: Option<u64>,
    ) -> Option<InactiveStateTimeoutFence> {
        let mut map = self.owners.lock().expect("state_timeout tracker poisoned");
        let owner = map.entry(key.clone()).or_default();
        if owner.generation != 0 {
            if event_order < owner.event_order {
                return None;
            }
            if event_order == owner.event_order && owner.declaration.is_none() {
                return Some(InactiveStateTimeoutFence {
                    generation: owner.generation,
                    event_order: owner.event_order,
                });
            }
        }
        Self::cancel_task(owner);
        let generation = self.next_generation();
        owner.generation = generation;
        owner.event_order = event_order;
        owner.reset_at = reset_at;
        owner.reset_version = reset_version;
        owner.declaration = None;
        Some(InactiveStateTimeoutFence {
            generation,
            event_order,
        })
    }

    /// Invalidate only the owner captured by a particular task.
    pub(super) fn invalidate_generation_if_current(
        &self,
        key: &EntityKey,
        armed_generation: u64,
    ) -> bool {
        let mut map = self.owners.lock().expect("state_timeout tracker poisoned");
        let Some(owner) = map.get_mut(key) else {
            return false;
        };
        if owner.generation != armed_generation {
            return false;
        }
        Self::cancel_task(owner);
        owner.generation = self.next_generation();
        owner.declaration = None;
        true
    }

    pub(super) fn current_generation(&self, key: &EntityKey) -> u64 {
        self.owners
            .lock()
            .expect("state_timeout tracker poisoned")
            .get(key)
            .map(|owner| owner.generation)
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(super) fn record_reconciliation_failure(&self) {
        let mut failures = self
            .reconciliation_failures
            .lock()
            .expect("state_timeout tracker poisoned");
        *failures = failures
            .checked_add(1)
            .expect("state timeout reconciliation failure counter overflow");
    }

    #[cfg(test)]
    pub(super) fn reconciliation_failure_count(&self) -> u64 {
        *self
            .reconciliation_failures
            .lock()
            .expect("state_timeout tracker poisoned")
    }

    #[cfg(test)]
    /// Pause one registry reconciliation after it captures authoritative IDs.
    pub(crate) fn pause_next_registry_entity_scan(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (captured, captured_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = self
            .registry_scan_gate
            .lock()
            .expect("registry scan gate poisoned");
        assert!(gate.is_none(), "only one registry scan gate may be armed");
        *gate = Some(RegistryScanGate {
            captured,
            release: release_rx,
        });
        (captured_rx, release)
    }

    #[cfg(test)]
    /// Enter and await the armed deterministic registry-scan test gate, if any.
    pub(crate) async fn wait_for_registry_entity_scan_release(&self) {
        let gate = self
            .registry_scan_gate
            .lock()
            .expect("registry scan gate poisoned")
            .take();
        if let Some(gate) = gate {
            let _ = gate.captured.send(());
            let _ = gate.release.await;
        }
    }

    /// Increment the pending-timer count for `entity_type`. Called at arm.
    pub fn inc_pending(&self, entity_type: &str) {
        let mut map = self
            .pending_by_type
            .lock()
            .expect("pending_by_type poisoned");
        *map.entry(entity_type.to_string()).or_insert(0) += 1;
    }

    /// Decrement the pending-timer count for `entity_type`. Called when a
    /// timer task exits (fired, cancelled by seq mismatch, or state changed).
    pub fn dec_pending(&self, entity_type: &str) {
        let mut map = self
            .pending_by_type
            .lock()
            .expect("pending_by_type poisoned");
        if let Some(value) = map.get_mut(entity_type)
            && *value > 0
        {
            *value -= 1;
        }
    }

    /// Snapshot pending counts per entity type for metric emission.
    pub fn pending_snapshot(&self) -> Vec<(String, u64)> {
        let map = self
            .pending_by_type
            .lock()
            .expect("pending_by_type poisoned");
        map.iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect()
    }

    /// Drop any owner for `key` after an entity is deleted.
    pub fn forget(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) {
        let key = EntityKey::new(tenant, entity_type, entity_id);
        let mut owners = self.owners.lock().expect("state_timeout tracker poisoned");
        if let Some(mut owner) = owners.remove(&key) {
            Self::cancel_task(&mut owner);
        }
    }

    pub(super) fn forget_inactive_if_current(
        &self,
        key: &EntityKey,
        fence: InactiveStateTimeoutFence,
    ) -> bool {
        let mut owners = self.owners.lock().expect("state_timeout tracker poisoned");
        if owners.get(key).is_none_or(|owner| {
            owner.declaration.is_some()
                || owner.generation != fence.generation
                || owner.event_order != fence.event_order
        }) {
            return false;
        }
        let mut owner = owners
            .remove(key)
            .expect("inactive timeout owner disappeared while locked");
        Self::cancel_task(&mut owner);
        true
    }

    #[cfg(test)]
    pub(crate) fn size(&self) -> usize {
        self.owners
            .lock()
            .expect("state_timeout tracker poisoned")
            .len()
    }
}
