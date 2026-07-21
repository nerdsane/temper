//! Reconcile durable entity timeout ownership after live table changes.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use temper_runtime::tenant::TenantId;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::registry::{RegistryTableChange, SpecRegistry};

use crate::state::{ServerState, StateTimeoutTracker};

const RECONCILIATION_RETRY_INITIAL: Duration = Duration::from_millis(100);
const RECONCILIATION_RETRY_MAX: Duration = Duration::from_secs(30);
const SERVER_LIFETIME_POLL: Duration = Duration::from_secs(1);
const TABLE_CHANGE_DRAIN_BUDGET: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReconciliationTarget {
    tenant: TenantId,
    entity_type: String,
}

impl From<&RegistryTableChange> for ReconciliationTarget {
    fn from(change: &RegistryTableChange) -> Self {
        Self {
            tenant: change.tenant.clone(),
            entity_type: change.entity_type.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct PendingReconciliation {
    version: u64,
    failure_count: u32,
    retry_at: tokio::time::Instant,
}

enum ReconciliationOutcome {
    Complete,
    Superseded,
    Retry(String),
}

impl ServerState {
    /// Start one coalescing registry-change consumer for this server state once.
    pub(in crate::state) fn ensure_registry_timeout_reconciliation_started(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                "registry timeout reconciliation will remain inactive without a Tokio runtime"
            );
            return;
        };
        let Some(lifetime) = self.state_timeout_tracker.begin_registry_reconciliation() else {
            return;
        };
        // Claim before scanning so every later hot-path call returns cheaply.
        // Subscribe and snapshot under one registry read after that claim. A
        // direct controller swap can still occur concurrently, but its
        // observer is already connected to this receiver.
        let (table_changes, initial_changes) = match self.registry.read() {
            Ok(registry) => (
                registry.subscribe_table_changes(),
                registry.timed_table_changes(),
            ),
            Err(_) => {
                tracing::error!(
                    "registry timeout reconciliation could not subscribe through a poisoned lock"
                );
                self.state_timeout_tracker.finish_registry_reconciliation();
                return;
            }
        };

        let live_registry = Arc::downgrade(&self.registry);
        let live_timeout_tracker = Arc::downgrade(&self.state_timeout_tracker);
        let live_server_lifetime = Arc::downgrade(&self.commons_write_guardrail_lock);

        // The permanent task must not retain its own sender, timeout tracker,
        // or server-lifetime token. Attach the live values only around one
        // bounded reconciliation attempt.
        let mut state_template = self.clone();
        state_template.registry = Arc::new(RwLock::new(SpecRegistry::new()));
        state_template.state_timeout_tracker = Arc::new(StateTimeoutTracker::new());
        state_template.commons_write_guardrail_lock = Arc::new(tokio::sync::Mutex::new(()));
        runtime.spawn(async move {
            // determinism-ok: one ordered owner coalesces bounded table signals
            run_registry_timeout_reconciliation(
                state_template,
                live_registry,
                live_timeout_tracker.clone(),
                live_server_lifetime,
                table_changes,
                lifetime,
                initial_changes,
            )
            .await;
            if let Some(tracker) = live_timeout_tracker.upgrade() {
                tracker.finish_registry_reconciliation();
            }
        });
    }

    async fn reconcile_registry_target(
        &self,
        target: &ReconciliationTarget,
        version: u64,
    ) -> ReconciliationOutcome {
        match self.registry_target_is_current_and_timed(target, version) {
            Ok(false) => return ReconciliationOutcome::Superseded,
            Err(error) => return ReconciliationOutcome::Retry(error),
            Ok(true) => {}
        }

        let memory_only = self.event_journal().is_none();
        let entity_ids = match self
            .fresh_entity_ids_for_timeout_reconciliation(&target.tenant, &target.entity_type)
            .await
        {
            Ok(entity_ids) => entity_ids,
            Err(error) => return ReconciliationOutcome::Retry(error),
        };
        #[cfg(test)]
        self.state_timeout_tracker
            .wait_for_registry_entity_scan_release()
            .await;
        for entity_id in entity_ids {
            match self.registry_target_is_current_and_timed(target, version) {
                Ok(false) => return ReconciliationOutcome::Superseded,
                Err(error) => return ReconciliationOutcome::Retry(error),
                Ok(true) => {}
            }
            let materialized = if memory_only {
                self.ensure_indexed_entity_actor_materialized(
                    &target.tenant,
                    &target.entity_type,
                    &entity_id,
                )
                .await
            } else {
                self.ensure_entity_actor_materialized(
                    &target.tenant,
                    &target.entity_type,
                    &entity_id,
                )
                .await
            };
            if !materialized
                && (!memory_only
                    || self.entity_exists(&target.tenant, &target.entity_type, &entity_id))
            {
                return ReconciliationOutcome::Retry(format!(
                    "could not materialize current entity {entity_id}"
                ));
            }
        }

        match self.registry_target_is_current_and_timed(target, version) {
            Ok(true) => ReconciliationOutcome::Complete,
            Ok(false) => ReconciliationOutcome::Superseded,
            Err(error) => ReconciliationOutcome::Retry(error),
        }
    }

    fn registry_target_is_current_and_timed(
        &self,
        target: &ReconciliationTarget,
        version: u64,
    ) -> Result<bool, String> {
        let registry = self
            .registry
            .read()
            .map_err(|error| format!("registry lock poisoned: {error}"))?;
        Ok(registry
            .get_spec(&target.tenant, &target.entity_type)
            .is_some_and(|spec| {
                spec.swap_controller().version() == version
                    && !spec.table().state_timeouts.is_empty()
            }))
    }

    async fn fresh_entity_ids_for_timeout_reconciliation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<Vec<String>, String> {
        let Some((store, _backend)) = self.event_journal() else {
            return Ok(self.list_entity_ids(tenant, entity_type));
        };
        let mut entity_ids = store
            .list_entity_ids_by_type(tenant.as_str(), entity_type)
            .await
            .map_err(|error| {
                format!("failed to list durable entities for {tenant}:{entity_type}: {error}")
            })?;
        entity_ids.sort();
        entity_ids.dedup();
        Ok(entity_ids)
    }
}

async fn run_registry_timeout_reconciliation(
    state_template: ServerState,
    live_registry: Weak<RwLock<SpecRegistry>>,
    live_timeout_tracker: Weak<StateTimeoutTracker>,
    live_server_lifetime: Weak<tokio::sync::Mutex<()>>,
    mut table_changes: tokio::sync::broadcast::Receiver<RegistryTableChange>,
    mut lifetime: tokio::sync::watch::Receiver<bool>,
    initial_changes: Vec<RegistryTableChange>,
) {
    let mut pending = BTreeMap::new();
    let now = tokio::time::Instant::now(); // determinism-ok: Tokio virtual clock
    for change in initial_changes {
        enqueue_change(&mut pending, change, now);
    }

    loop {
        if *lifetime.borrow() || live_server_lifetime.upgrade().is_none() {
            break;
        }
        if !drain_table_changes(&mut table_changes, &live_registry, &mut pending) {
            break;
        }

        let now = tokio::time::Instant::now(); // determinism-ok: Tokio virtual clock
        if let Some(target) = next_due_target(&pending, now) {
            let Some(registry) = live_registry.upgrade() else {
                break;
            };
            let Some(tracker) = live_timeout_tracker.upgrade() else {
                break;
            };
            let mut state = state_template.clone();
            state.registry = registry;
            state.state_timeout_tracker = tracker;
            let version = pending
                .get(&target)
                .expect("due reconciliation target disappeared")
                .version;
            match state.reconcile_registry_target(&target, version).await {
                ReconciliationOutcome::Complete => {
                    pending.remove(&target);
                }
                ReconciliationOutcome::Superseded => {
                    // Controller-local versions restart at one when a type is
                    // removed and re-added. Replace the stale target from the
                    // live registry instead of ordering across lifetimes.
                    if !replace_with_current_target(&live_registry, &mut pending, &target) {
                        break;
                    }
                }
                ReconciliationOutcome::Retry(error) => {
                    let entry = pending
                        .get_mut(&target)
                        .expect("retry reconciliation target disappeared");
                    entry.failure_count = entry.failure_count.saturating_add(1);
                    let retry_delay = reconciliation_retry_delay(entry.failure_count);
                    entry.retry_at = tokio::time::Instant::now() + retry_delay; // determinism-ok: Tokio virtual clock
                    tracing::warn!(
                        tenant = %target.tenant,
                        entity_type = target.entity_type,
                        version,
                        failure_count = entry.failure_count,
                        retry_delay_ms = retry_delay.as_millis() as u64,
                        error,
                        "table-change timeout reconciliation will retry"
                    );
                }
            }
            continue;
        }

        let heartbeat = tokio::time::Instant::now() + SERVER_LIFETIME_POLL; // determinism-ok: Tokio virtual clock
        let deadline = pending
            .values()
            .map(|entry| entry.retry_at)
            .min()
            .map_or(heartbeat, |retry_at| retry_at.min(heartbeat));
        tokio::select! { // determinism-ok: ordered signal, shutdown, and retry ownership
            biased;
            changed = lifetime.changed() => {
                if changed.is_err() || *lifetime.borrow() {
                    break;
                }
            }
            received = table_changes.recv() => {
                if !handle_received_change(received, &live_registry, &mut pending) {
                    break;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}

fn enqueue_change(
    pending: &mut BTreeMap<ReconciliationTarget, PendingReconciliation>,
    change: RegistryTableChange,
    now: tokio::time::Instant,
) {
    let target = ReconciliationTarget::from(&change);
    match pending.get_mut(&target) {
        Some(existing) if existing.version >= change.version => {}
        Some(existing) => {
            *existing = PendingReconciliation {
                version: change.version,
                failure_count: 0,
                retry_at: now,
            };
        }
        None => {
            pending.insert(
                target,
                PendingReconciliation {
                    version: change.version,
                    failure_count: 0,
                    retry_at: now,
                },
            );
        }
    }
}

fn next_due_target(
    pending: &BTreeMap<ReconciliationTarget, PendingReconciliation>,
    now: tokio::time::Instant,
) -> Option<ReconciliationTarget> {
    pending
        .iter()
        .filter(|(_, entry)| entry.retry_at <= now)
        .min_by_key(|(target, entry)| (entry.retry_at, *target))
        .map(|(target, _)| target.clone())
}

fn drain_table_changes(
    table_changes: &mut tokio::sync::broadcast::Receiver<RegistryTableChange>,
    live_registry: &Weak<RwLock<SpecRegistry>>,
    pending: &mut BTreeMap<ReconciliationTarget, PendingReconciliation>,
) -> bool {
    for _ in 0..TABLE_CHANGE_DRAIN_BUDGET {
        match table_changes.try_recv() {
            Ok(change) => enqueue_change(pending, change, tokio::time::Instant::now()), // determinism-ok: Tokio virtual clock
            Err(TryRecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "registry timeout reconciliation lagged; rebuilding current timed targets"
                );
                if !replace_with_current_targets(live_registry, pending) {
                    return false;
                }
            }
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Closed) => return false,
        }
    }
    true
}

fn handle_received_change(
    received: Result<RegistryTableChange, RecvError>,
    live_registry: &Weak<RwLock<SpecRegistry>>,
    pending: &mut BTreeMap<ReconciliationTarget, PendingReconciliation>,
) -> bool {
    match received {
        Ok(change) => {
            enqueue_change(pending, change, tokio::time::Instant::now()); // determinism-ok: Tokio virtual clock
            true
        }
        Err(RecvError::Lagged(skipped)) => {
            tracing::warn!(
                skipped,
                "registry timeout reconciliation lagged; rebuilding current timed targets"
            );
            replace_with_current_targets(live_registry, pending)
        }
        Err(RecvError::Closed) => false,
    }
}

fn replace_with_current_targets(
    live_registry: &Weak<RwLock<SpecRegistry>>,
    pending: &mut BTreeMap<ReconciliationTarget, PendingReconciliation>,
) -> bool {
    let Some(registry) = live_registry.upgrade() else {
        return false;
    };
    let Ok(registry) = registry.read() else {
        tracing::error!("registry timeout reconciliation cannot recover a lagged poisoned lock");
        return false;
    };
    let changes = registry.timed_table_changes();
    drop(registry);
    pending.clear();
    let now = tokio::time::Instant::now(); // determinism-ok: Tokio virtual clock
    for change in changes {
        enqueue_change(pending, change, now);
    }
    true
}

fn replace_with_current_target(
    live_registry: &Weak<RwLock<SpecRegistry>>,
    pending: &mut BTreeMap<ReconciliationTarget, PendingReconciliation>,
    target: &ReconciliationTarget,
) -> bool {
    let Some(registry) = live_registry.upgrade() else {
        return false;
    };
    let Ok(registry) = registry.read() else {
        tracing::error!(
            tenant = %target.tenant,
            entity_type = target.entity_type,
            "registry timeout reconciliation cannot refresh a superseded poisoned target"
        );
        return false;
    };
    let change = registry.timed_table_change(&target.tenant, &target.entity_type);
    drop(registry);

    pending.remove(target);
    if let Some(change) = change {
        enqueue_change(pending, change, tokio::time::Instant::now()); // determinism-ok: Tokio virtual clock
    }
    true
}

fn reconciliation_retry_delay(failure_count: u32) -> Duration {
    let shift = failure_count.saturating_sub(1).min(8);
    RECONCILIATION_RETRY_INITIAL
        .checked_mul(1_u32 << shift)
        .unwrap_or(RECONCILIATION_RETRY_MAX)
        .min(RECONCILIATION_RETRY_MAX)
}

#[cfg(test)]
mod tests {
    use temper_jit::table::TransitionTable;
    use temper_runtime::ActorSystem;
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_spec::csdl::CsdlDocument;

    use super::*;

    const UNTIMED_IOA: &str = r#"
[automaton]
name = "Untimed"
states = ["Ready"]
initial = "Ready"
allow_indefinite_states = ["Ready"]
"#;

    #[tokio::test(start_paused = true)]
    async fn detached_registry_and_controller_do_not_retain_server_worker() {
        let (_guard, _clock, _ids) = install_deterministic_context(238);
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
            &[("Untimed", UNTIMED_IOA)],
        );
        let state =
            ServerState::from_registry(ActorSystem::new("registry-worker-lifetime"), registry);
        state.ensure_registry_timeout_reconciliation_started();
        let tracker = state.state_timeout_tracker.clone();
        assert!(
            tracker.begin_registry_reconciliation().is_none(),
            "the server owns exactly one reconciliation worker"
        );
        let detached_registry = state.registry.clone();
        let detached_controller = detached_registry
            .read()
            .expect("registry lock")
            .get_spec(&TenantId::default(), "Untimed")
            .expect("registered spec")
            .swap_controller()
            .clone();

        drop(state);
        tokio::time::advance(SERVER_LIFETIME_POLL).await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
            if tracker.begin_registry_reconciliation().is_some() {
                tracker.finish_registry_reconciliation();
                return;
            }
        }

        let _ = detached_controller.swap(TransitionTable::from_ioa_source(UNTIMED_IOA));
        panic!("detached registry/controller retained the server reconciliation worker");
    }
}
