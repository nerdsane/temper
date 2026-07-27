//! Deterministic in-memory platform store and fault injector.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use temper_store_sim::DeterministicRng;

mod quarantine;
mod store;

/// Fault injection configuration for platform store simulation.
///
/// Controls the probability of injected failures during platform store
/// operations. All probabilities are in \[0.0, 1.0\].
#[derive(Debug, Clone)]
pub struct SimPlatformFaultConfig {
    /// Probability of a write failure on spec upsert.
    pub spec_write_failure_prob: f64,
    /// Probability of a read failure on spec load.
    pub spec_read_failure_prob: f64,
    /// Probability of a durable quarantine reconciliation failure.
    pub quarantine_write_failure_prob: f64,
    /// Number of quarantine CAS attempts that first advance a committed source version.
    pub registry_source_drift_budget: usize,
    /// Probability of an active quarantine read failure.
    pub quarantine_read_failure_prob: f64,
    /// Probability of a write failure on policy upsert.
    pub policy_write_failure_prob: f64,
    /// Probability of a read failure on policy load.
    pub policy_read_failure_prob: f64,
    /// Probability of a failure recording an installed app.
    pub app_record_failure_prob: f64,
    /// Probability of a failure listing installed apps.
    pub app_list_failure_prob: f64,
    /// Probability of a write failure on pending decision upsert.
    pub decision_write_failure_prob: f64,
    /// Probability of a read failure on pending decision load.
    pub decision_read_failure_prob: f64,
    /// Probability of a failure when deleting a spec (cleanup path).
    pub cleanup_failure_prob: f64,
    /// Probability of a read failure on WASM module load.
    pub wasm_read_failure_prob: f64,
}

impl SimPlatformFaultConfig {
    /// No fault injection — all operations succeed.
    pub fn none() -> Self {
        Self {
            spec_write_failure_prob: 0.0,
            spec_read_failure_prob: 0.0,
            quarantine_write_failure_prob: 0.0,
            registry_source_drift_budget: 0,
            quarantine_read_failure_prob: 0.0,
            policy_write_failure_prob: 0.0,
            policy_read_failure_prob: 0.0,
            app_record_failure_prob: 0.0,
            app_list_failure_prob: 0.0,
            decision_write_failure_prob: 0.0,
            decision_read_failure_prob: 0.0,
            cleanup_failure_prob: 0.0,
            wasm_read_failure_prob: 0.0,
        }
    }

    /// Heavy fault injection for stress testing.
    pub fn heavy() -> Self {
        Self {
            spec_write_failure_prob: 0.05,
            spec_read_failure_prob: 0.02,
            quarantine_write_failure_prob: 0.03,
            registry_source_drift_budget: 0,
            quarantine_read_failure_prob: 0.02,
            policy_write_failure_prob: 0.05,
            policy_read_failure_prob: 0.02,
            app_record_failure_prob: 0.03,
            app_list_failure_prob: 0.02,
            decision_write_failure_prob: 0.04,
            decision_read_failure_prob: 0.02,
            cleanup_failure_prob: 0.03,
            wasm_read_failure_prob: 0.02,
        }
    }
}

impl Default for SimPlatformFaultConfig {
    fn default() -> Self {
        Self::none()
    }
}

/// In-memory, deterministic platform store for DST.
///
/// Implements [`PlatformStore`] trait. All operations resolve immediately.
/// Fault injection controlled by [`DeterministicRng`].
///
/// Uses ordered maps and sets exclusively for deterministic iteration order.
#[derive(Clone)]
pub struct SimPlatformStore {
    inner: Arc<Mutex<SimPlatformStoreInner>>,
}

struct SimPlatformStoreInner {
    /// Deterministic RNG for fault injection.
    rng: DeterministicRng,
    /// Fault injection configuration.
    faults: SimPlatformFaultConfig,
    /// Specs keyed by (tenant, entity_type).
    specs: BTreeMap<(String, String), SpecRow>,
    /// Verification cache: (tenant, entity_type) -> (content_hash, verified).
    verification_cache: BTreeMap<(String, String), (String, bool)>,
    /// Active and resolved durable registry restore quarantine history.
    registry_quarantines: BTreeMap<(String, String, i64, i64), SimRegistryQuarantineEntry>,
    /// Cedar policies keyed by tenant.
    policies: BTreeMap<String, String>,
    /// Granular Cedar policy rows keyed by (tenant, policy_id).
    policy_entries: BTreeMap<(String, String), PolicyEntryRow>,
    /// Versioned cross-invariant definitions keyed by tenant.
    constraints: BTreeMap<String, (String, i64)>,
    /// Installed apps: (tenant, app_name).
    installed_apps: BTreeSet<(String, String)>,
    /// Installed app digest metadata keyed by (tenant, app_name).
    installed_app_records: BTreeMap<(String, String), InstalledAppRecord>,
    /// Pending decisions: id -> JSON data.
    pending_decisions: BTreeMap<String, (String, String, String)>,
    /// WASM modules keyed by (tenant, module_name).
    wasm_modules: BTreeMap<(String, String), WasmModuleRow>,
}

struct SimRegistryQuarantineEntry {
    record: RegistryQuarantineRecord,
    resolved: bool,
}

impl SimPlatformStore {
    /// Create a new `SimPlatformStore` with the given seed and fault config.
    pub fn new(seed: u64, faults: SimPlatformFaultConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SimPlatformStoreInner {
                rng: DeterministicRng::new(seed),
                faults,
                specs: BTreeMap::new(),
                verification_cache: BTreeMap::new(),
                registry_quarantines: BTreeMap::new(),
                policies: BTreeMap::new(),
                policy_entries: BTreeMap::new(),
                constraints: BTreeMap::new(),
                installed_apps: BTreeSet::new(),
                installed_app_records: BTreeMap::new(),
                pending_decisions: BTreeMap::new(),
                wasm_modules: BTreeMap::new(),
            })),
        }
    }

    /// Create a `SimPlatformStore` with no fault injection.
    pub fn no_faults(seed: u64) -> Self {
        Self::new(seed, SimPlatformFaultConfig::none())
    }

    /// Temporarily disable all fault injection.
    ///
    /// Returns the previous config so it can be restored. Useful for
    /// invariant checks that must read the store reliably.
    pub fn disable_faults(&self) -> SimPlatformFaultConfig {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
        let prev = inner.faults.clone();
        inner.faults = SimPlatformFaultConfig::none();
        prev
    }

    /// Restore a previously saved fault config.
    pub fn restore_faults(&self, faults: SimPlatformFaultConfig) {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
        inner.faults = faults;
    }

    /// Seed a granular policy row for recovery tests.
    pub fn insert_policy_entry_for_test(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        enabled: bool,
    ) {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
        inner.policy_entries.insert(
            (tenant.to_string(), policy_id.to_string()),
            PolicyEntryRow {
                tenant: tenant.to_string(),
                policy_id: policy_id.to_string(),
                cedar_text: cedar_text.to_string(),
                enabled,
            },
        );
    }
}

impl std::fmt::Debug for SimPlatformStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
        f.debug_struct("SimPlatformStore")
            .field("specs", &inner.specs.len())
            .field("policies", &inner.policies.len())
            .field("policy_entries", &inner.policy_entries.len())
            .field("installed_apps", &inner.installed_apps.len())
            .field("wasm_modules", &inner.wasm_modules.len())
            .finish()
    }
}
