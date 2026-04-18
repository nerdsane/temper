//! Admission control for action dispatch (ADR-0051).
//!
//! Gates concurrent `ActorRef::ask` calls per `(tenant, entity_type, action)`
//! tuple so bursty workloads queue fairly instead of producing mailbox-full
//! storms and ask timeouts.
//!
//! ## Shape
//!
//! - Caps are spec-declared via `[admission]` blocks on entity specs
//!   (`temper-spec::automaton::types::Admission`) and registered here at
//!   spec-load time via [`AdmissionController::register_spec`].
//! - A semaphore is created per `(TenantId, entity_type, action)` on first
//!   use. Permits are per-tenant — one tenant's saturation does not starve
//!   another.
//! - FIFO acquisition order is a contract-level invariant. Tokio's
//!   `Semaphore::acquire_owned` is fair; the test at
//!   `fifo_acquisition_order` pins the guarantee.
//! - If no `[admission]` block is declared for the entity, the dispatch
//!   layer passes through (no permit, no gating). Backward-compatible.
//!
//! ## Runtime override
//!
//! [`AdmissionController::override_caps`] replaces an entity's caps at
//! runtime so SRE can tune capacity without redeploy. Existing in-flight
//! permits are not revoked — the new limit applies to future acquisitions.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Admission;

/// Default queue timeout when an `[admission]` block omits one.
pub const DEFAULT_QUEUE_TIMEOUT_SECS: u64 = 30;

/// Per-entity admission configuration, materialized from a spec `[admission]`
/// block with defaults applied.
#[derive(Debug, Clone)]
struct EntityCaps {
    /// Cap for the `Create` action. None = no gating for creates.
    max_concurrent_creates: Option<u32>,
    /// Per-action caps. None for an action = no gating for that action.
    max_concurrent_actions: BTreeMap<String, u32>,
    /// Queue wait before returning `Deferred`.
    queue_timeout: Duration,
}

impl EntityCaps {
    fn cap_for_action(&self, action: &str) -> Option<u32> {
        if action == "Create" {
            self.max_concurrent_creates
        } else {
            self.max_concurrent_actions.get(action).copied()
        }
    }

    /// Re-export the caps as an `Admission` value so the registered-caps
    /// path can share the same entry point as the inline-caps path.
    fn as_admission(&self) -> Admission {
        Admission {
            max_concurrent_creates: self.max_concurrent_creates,
            max_concurrent_actions: self
                .max_concurrent_actions
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            queue_depth: None,
            queue_timeout_seconds: Some(self.queue_timeout.as_secs() as u32),
        }
    }
}

impl From<Admission> for EntityCaps {
    fn from(value: Admission) -> Self {
        let queue_timeout = Duration::from_secs(
            value
                .queue_timeout_seconds
                .map(|v| v as u64)
                .unwrap_or(DEFAULT_QUEUE_TIMEOUT_SECS),
        );
        Self {
            max_concurrent_creates: value.max_concurrent_creates,
            max_concurrent_actions: value.max_concurrent_actions.into_iter().collect(),
            queue_timeout,
        }
    }
}

/// Identifier for a single semaphore slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SemaphoreKey {
    tenant: TenantId,
    entity_type: String,
    action: String,
}

#[derive(Default)]
struct Inner {
    /// Caps keyed by entity type (kept for `try_acquire` without inline
    /// caps + inspection helpers).
    caps: HashMap<String, EntityCaps>,
    /// Semaphores, created lazily on first request for a key. Each entry
    /// remembers the permit count it was built for so a cap change rebuilds
    /// the semaphore instead of silently using a stale max.
    semaphores: HashMap<SemaphoreKey, SemaphoreEntry>,
}

struct SemaphoreEntry {
    semaphore: Arc<Semaphore>,
    built_for_cap: u32,
    /// Pending acquirers waiting on this semaphore (queue depth). Incremented
    /// before each `acquire_owned`, decremented after grant or timeout.
    pending: Arc<AtomicU64>,
}

/// Admission controller owned by `ServerState`.
#[derive(Default)]
pub struct AdmissionController {
    inner: Mutex<Inner>,
}

/// Outcome of an acquisition attempt.
pub enum AdmissionOutcome {
    /// No cap declared for this `(entity_type, action)` — no permit needed.
    Passthrough,
    /// Permit granted. Dropping the permit releases the slot.
    Granted(AdmissionPermit),
    /// Acquisition waited `queue_timeout` without obtaining a permit.
    Deferred {
        retry_after_ms: u64,
        waited: Duration,
    },
}

/// RAII permit. Dropping it releases the semaphore slot and emits
/// `temper_admission_permit_hold_time_ms`.
pub struct AdmissionPermit {
    _permit: OwnedSemaphorePermit,
    tenant: String,
    entity_type: String,
    action: String,
    acquired_at: Instant,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let held = self.acquired_at.elapsed();
        crate::runtime_metrics::record_admission_permit_hold(
            &self.tenant,
            &self.entity_type,
            &self.action,
            held,
        );
    }
}

impl AdmissionController {
    /// Construct an empty controller. No caps are registered until
    /// [`register_spec`] is called for each spec.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or refresh) an entity's admission caps from its spec.
    ///
    /// Called during spec load. Replacing caps does not revoke existing
    /// permits; the new cap applies to future acquisitions. Existing
    /// semaphores for the entity are invalidated so the next acquisition
    /// rebuilds with the new permit count.
    pub async fn register_spec(&self, entity_type: &str, admission: Option<&Admission>) {
        let mut inner = self.inner.lock().await;
        match admission {
            Some(a) => {
                let caps = EntityCaps::from(a.clone());
                inner.caps.insert(entity_type.to_string(), caps);
            }
            None => {
                inner.caps.remove(entity_type);
            }
        }
        // Drop any cached semaphores for this entity so new caps take effect.
        // Existing borrowed permits stay alive on their original Arc<Semaphore>
        // and release normally.
        inner
            .semaphores
            .retain(|k, _| k.entity_type != entity_type);
    }

    /// Runtime override: replace caps for an entity without a redeploy.
    /// Used by the ops `/_admin/admission/...` endpoint.
    pub async fn override_caps(&self, entity_type: &str, admission: Option<Admission>) {
        self.register_spec(entity_type, admission.as_ref()).await;
    }

    /// Attempt to acquire a permit for this call using an inline caps
    /// reference (typically pulled from the spec registry on each call).
    ///
    /// Semantics:
    /// - `None` caps → `Passthrough` immediately (no gating for this entity).
    /// - Caps with no entry for `action` → `Passthrough`.
    /// - Otherwise waits up to the entity's `queue_timeout` for a permit.
    ///
    /// Tokio's semaphore is FIFO — waiters are served in arrival order.
    pub async fn try_acquire_with_caps(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
        admission: Option<&Admission>,
    ) -> AdmissionOutcome {
        let Some(admission) = admission else {
            return AdmissionOutcome::Passthrough;
        };
        let caps: EntityCaps = admission.clone().into();
        let Some(max) = caps.cap_for_action(action) else {
            return AdmissionOutcome::Passthrough;
        };

        let start = Instant::now();
        let key = SemaphoreKey {
            tenant: tenant.clone(),
            entity_type: entity_type.to_string(),
            action: action.to_string(),
        };

        // Refresh cached caps so a cap change takes effect on subsequent
        // acquisitions. Existing borrowed permits release against their
        // original Arc<Semaphore> naturally.
        let (semaphore, pending) = {
            let mut inner = self.inner.lock().await;
            inner.caps.insert(entity_type.to_string(), caps.clone());
            let needs_rebuild = inner
                .semaphores
                .get(&key)
                .is_none_or(|e| e.built_for_cap != max);
            if needs_rebuild {
                inner.semaphores.insert(
                    key.clone(),
                    SemaphoreEntry {
                        semaphore: Arc::new(Semaphore::new(max as usize)),
                        built_for_cap: max,
                        pending: Arc::new(AtomicU64::new(0)),
                    },
                );
            }
            let entry = inner.semaphores.get(&key).unwrap();
            (entry.semaphore.clone(), entry.pending.clone())
        };
        let queue_timeout = caps.queue_timeout;

        pending.fetch_add(1, Ordering::AcqRel);
        let outcome = tokio::time::timeout(queue_timeout, semaphore.clone().acquire_owned()).await;
        pending.fetch_sub(1, Ordering::AcqRel);

        match outcome {
            Ok(Ok(permit)) => AdmissionOutcome::Granted(AdmissionPermit {
                _permit: permit,
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                action: action.to_string(),
                acquired_at: start,
            }),
            Ok(Err(_closed)) => AdmissionOutcome::Passthrough,
            Err(_elapsed) => AdmissionOutcome::Deferred {
                retry_after_ms: queue_timeout.as_millis() as u64,
                waited: start.elapsed(),
            },
        }
    }

    /// Snapshot the current per-`(tenant, entity_type, action)` semaphore
    /// state for gauge emission. Called from the canary metrics loop.
    ///
    /// Returns `(tenant, entity_type, action, active_permits, queue_depth)`.
    pub async fn snapshot_for_metrics(&self) -> Vec<(String, String, String, u64, u64)> {
        let inner = self.inner.lock().await;
        inner
            .semaphores
            .iter()
            .map(|(key, entry)| {
                let available = entry.semaphore.available_permits() as u64;
                let active = (entry.built_for_cap as u64).saturating_sub(available);
                let queue_depth = entry.pending.load(Ordering::Acquire);
                (
                    key.tenant.to_string(),
                    key.entity_type.clone(),
                    key.action.clone(),
                    active,
                    queue_depth,
                )
            })
            .collect()
    }

    /// Attempt to acquire using only registered caps (no inline reference).
    /// Useful for tests and callers that pre-registered via
    /// [`register_spec`].
    pub async fn try_acquire(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
    ) -> AdmissionOutcome {
        let registered_admission = {
            let inner = self.inner.lock().await;
            inner.caps.get(entity_type).map(|c| c.as_admission())
        };
        self.try_acquire_with_caps(tenant, entity_type, action, registered_admission.as_ref())
            .await
    }

    #[cfg(test)]
    async fn registered_entity_count(&self) -> usize {
        self.inner.lock().await.caps.len()
    }

    #[cfg(test)]
    async fn semaphore_count(&self) -> usize {
        self.inner.lock().await.semaphores.len()
    }
}

impl crate::state::ServerState {
    /// Pull the admission block for an entity type from the registry.
    /// Returns `None` if the tenant or entity type is not registered, or
    /// the spec declares no `[admission]` block.
    pub(crate) fn admission_caps_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<Admission> {
        let registry = self.registry.read().ok()?;
        registry
            .get_spec(tenant, entity_type)?
            .automaton
            .admission
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tenant(name: &str) -> TenantId {
        TenantId::from(name.to_string())
    }

    fn make_admission(
        creates: Option<u32>,
        action_caps: &[(&str, u32)],
        queue_timeout_s: Option<u32>,
    ) -> Admission {
        Admission {
            max_concurrent_creates: creates,
            max_concurrent_actions: action_caps
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            queue_depth: None,
            queue_timeout_seconds: queue_timeout_s,
        }
    }

    #[tokio::test]
    async fn passthrough_when_no_admission_registered() {
        let ac = AdmissionController::new();
        let t = tenant("acme");
        match ac.try_acquire(&t, "Session", "Configure").await {
            AdmissionOutcome::Passthrough => {}
            _ => panic!("expected Passthrough"),
        }
    }

    #[tokio::test]
    async fn passthrough_when_admission_lacks_cap_for_action() {
        let ac = AdmissionController::new();
        // Register caps for Submit only.
        ac.register_spec(
            "Session",
            Some(&make_admission(None, &[("Submit", 2)], Some(1))),
        )
        .await;
        // Configure has no cap — should pass through.
        match ac.try_acquire(&tenant("x"), "Session", "Configure").await {
            AdmissionOutcome::Passthrough => {}
            _ => panic!("expected Passthrough for uncapped action"),
        }
    }

    #[tokio::test]
    async fn grants_up_to_cap_and_defers_beyond() {
        let ac = Arc::new(AdmissionController::new());
        ac.register_spec(
            "Session",
            Some(&make_admission(None, &[("Submit", 2)], Some(1))),
        )
        .await;
        let t = tenant("acme");

        // First two acquire immediately.
        let p1 = match ac.try_acquire(&t, "Session", "Submit").await {
            AdmissionOutcome::Granted(p) => p,
            _ => panic!("first acquire must grant"),
        };
        let p2 = match ac.try_acquire(&t, "Session", "Submit").await {
            AdmissionOutcome::Granted(p) => p,
            _ => panic!("second acquire must grant"),
        };

        // Third acquirer hits the queue_timeout and is deferred.
        match ac.try_acquire(&t, "Session", "Submit").await {
            AdmissionOutcome::Deferred { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, 1000, "retry_after_ms should match queue_timeout");
            }
            _ => panic!("third acquire must defer"),
        }

        // Once a permit is released, a fresh acquire should succeed.
        drop(p1);
        match ac.try_acquire(&t, "Session", "Submit").await {
            AdmissionOutcome::Granted(_) => {}
            _ => panic!("released permit must allow new acquisition"),
        }
        drop(p2);
    }

    #[tokio::test]
    async fn per_tenant_isolation() {
        let ac = AdmissionController::new();
        ac.register_spec(
            "Session",
            Some(&make_admission(None, &[("Submit", 1)], Some(1))),
        )
        .await;
        let a = tenant("alpha");
        let b = tenant("beta");

        // Alpha takes its one permit.
        let _p_alpha = match ac.try_acquire(&a, "Session", "Submit").await {
            AdmissionOutcome::Granted(p) => p,
            _ => panic!("alpha must get its permit"),
        };
        // Beta's permit count is independent — must grant.
        match ac.try_acquire(&b, "Session", "Submit").await {
            AdmissionOutcome::Granted(_) => {}
            _ => panic!("beta must not share alpha's cap"),
        }
    }

    // FIFO acquisition order is a contract-level guarantee (ADR-0051
    // sub-decision 4). This test interleaves many acquirers and asserts
    // grant order matches arrival order.
    #[tokio::test]
    async fn fifo_acquisition_order() {
        let ac = Arc::new(AdmissionController::new());
        ac.register_spec(
            "Session",
            // Cap of 1 so every acquirer must queue.
            Some(&make_admission(None, &[("Submit", 1)], Some(30))),
        )
        .await;
        let t = tenant("t");

        // Take the one permit so all subsequent acquirers queue.
        let holder = match ac.try_acquire(&t, "Session", "Submit").await {
            AdmissionOutcome::Granted(p) => p,
            _ => panic!("initial acquirer must grant"),
        };

        const WAITERS: usize = 16;
        let grant_order = Arc::new(Mutex::new(Vec::<usize>::new()));
        let arrived = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..WAITERS {
            let ac_c = ac.clone();
            let t_c = t.clone();
            let grant_order = grant_order.clone();
            let arrived = arrived.clone();
            let handle = tokio::spawn(async move {
                // Stagger arrivals deterministically so arrival order
                // matches spawn index, exercising the FIFO contract.
                while arrived.load(Ordering::Acquire) < i {
                    tokio::task::yield_now().await;
                }
                arrived.fetch_add(1, Ordering::AcqRel);

                match ac_c.try_acquire(&t_c, "Session", "Submit").await {
                    AdmissionOutcome::Granted(permit) => {
                        grant_order.lock().await.push(i);
                        // Hold for a short tick so the next waiter is the
                        // one released in FIFO order.
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        drop(permit);
                    }
                    other => panic!("waiter {i} expected Granted, got {other:?}"),
                }
            });
            handles.push(handle);
        }

        // Wait for all arrivals, then release the holder so the queue drains.
        while arrived.load(Ordering::Acquire) < WAITERS {
            tokio::task::yield_now().await;
        }
        drop(holder);

        for h in handles {
            h.await.unwrap();
        }

        let order = grant_order.lock().await.clone();
        assert_eq!(order.len(), WAITERS);
        for (expected, actual) in (0..WAITERS).zip(order.iter()) {
            assert_eq!(
                &expected, actual,
                "FIFO broken — expected {expected} at this position, got {actual}. Full order: {order:?}"
            );
        }
    }

    #[tokio::test]
    async fn register_refreshes_cap_without_affecting_inflight_permit() {
        let ac = AdmissionController::new();
        ac.register_spec(
            "Session",
            Some(&make_admission(None, &[("Submit", 1)], Some(1))),
        )
        .await;
        let t = tenant("acme");
        let _p = match ac.try_acquire(&t, "Session", "Submit").await {
            AdmissionOutcome::Granted(p) => p,
            _ => panic!("must acquire"),
        };
        // Re-register with a bigger cap. Semaphore cache is invalidated.
        ac.register_spec(
            "Session",
            Some(&make_admission(None, &[("Submit", 5)], Some(1))),
        )
        .await;
        // Since the old semaphore was evicted, the new acquire builds a
        // fresh Semaphore(5) that doesn't see the old permit. This is the
        // documented behavior; the old permit drops naturally when its
        // scope ends.
        match ac.try_acquire(&t, "Session", "Submit").await {
            AdmissionOutcome::Granted(_) => {}
            _ => panic!("new acquire after override must grant"),
        }
    }

    #[tokio::test]
    async fn unregister_removes_entity_caps() {
        let ac = AdmissionController::new();
        ac.register_spec(
            "Session",
            Some(&make_admission(None, &[("Submit", 1)], Some(1))),
        )
        .await;
        assert_eq!(ac.registered_entity_count().await, 1);
        let _p = match ac.try_acquire(&tenant("x"), "Session", "Submit").await {
            AdmissionOutcome::Granted(p) => p,
            _ => panic!("acquire"),
        };
        assert_eq!(ac.semaphore_count().await, 1);

        // Passing None unregisters and drops cached semaphores.
        ac.register_spec("Session", None).await;
        assert_eq!(ac.registered_entity_count().await, 0);
        assert_eq!(ac.semaphore_count().await, 0);

        match ac.try_acquire(&tenant("x"), "Session", "Submit").await {
            AdmissionOutcome::Passthrough => {}
            _ => panic!("unregistered entity must pass through"),
        }
    }

    impl std::fmt::Debug for AdmissionOutcome {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                AdmissionOutcome::Passthrough => write!(f, "Passthrough"),
                AdmissionOutcome::Granted(_) => write!(f, "Granted"),
                AdmissionOutcome::Deferred { retry_after_ms, .. } => {
                    write!(f, "Deferred {{ retry_after_ms: {retry_after_ms} }}")
                }
            }
        }
    }
}
