//! Deterministic in-memory platform store for simulation.

mod platform_store_impl;

#[cfg(test)]
mod tests;

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use temper_runtime::scheduler::sim_now;
use temper_store_sim::DeterministicRng;

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
/// Uses `BTreeMap`/`BTreeSet` exclusively (no `HashMap`/`HashSet`) for
/// deterministic iteration order.
#[derive(Clone)]
pub struct SimPlatformStore {
    inner: Arc<Mutex<SimPlatformStoreInner>>,
}

struct SimPlatformStoreInner {
    /// Deterministic RNG for fault injection.
    rng: DeterministicRng,
    /// Fault injection configuration.
    faults: SimPlatformFaultConfig,
    /// Number of upcoming atomic spec publications that fail before any
    /// durable mutation.
    pending_spec_publication_failures: usize,
    /// Number of upcoming atomic publications that durably commit but
    /// return an outcome-ambiguous error to the caller.
    pending_spec_publication_postcommit_failures: usize,
    /// Specs keyed by (tenant, entity_type).
    specs: BTreeMap<(String, String), SpecRow>,
    /// Verification cache: (tenant, entity_type) -> (content_hash, verified).
    verification_cache: BTreeMap<(String, String), (String, bool)>,
    /// Cedar policies keyed by tenant.
    policies: BTreeMap<String, String>,
    /// Granular Cedar policy rows keyed by (tenant, policy_id).
    policy_entries: BTreeMap<(String, String), PolicyEntryRow>,
    /// Cross-invariant definitions keyed by tenant.
    constraints: BTreeMap<String, String>,
    /// Installed apps: (tenant, app_name).
    installed_apps: BTreeSet<(String, String)>,
    /// Installed app digest metadata keyed by (tenant, app_name).
    installed_app_records: BTreeMap<(String, String), InstalledAppRecord>,
    /// Pending decisions: id -> JSON data.
    pending_decisions: BTreeMap<String, (String, String, String)>,
    /// WASM modules keyed by (tenant, module_name).
    wasm_modules: BTreeMap<(String, String), WasmModuleRow>,
}

impl SimPlatformStore {
    /// Create a new `SimPlatformStore` with the given seed and fault config.
    pub fn new(seed: u64, faults: SimPlatformFaultConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SimPlatformStoreInner {
                rng: DeterministicRng::new(seed),
                faults,
                pending_spec_publication_failures: 0,
                pending_spec_publication_postcommit_failures: 0,
                specs: BTreeMap::new(),
                verification_cache: BTreeMap::new(),
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

    /// Fail the next `count` atomic spec publications before mutation.
    /// `count == 0` clears the deterministic fault.
    pub fn fail_next_spec_publications(&self, count: usize) {
        self.inner
            .lock()
            .expect("SimPlatformStore lock poisoned") // ci-ok: infallible lock
            .pending_spec_publication_failures = count;
    }

    /// Commit the next `count` atomic spec publications, then return an
    /// error as if the commit acknowledgement were lost. `count == 0`
    /// clears the deterministic fault.
    pub fn fail_next_spec_publications_after_commit(&self, count: usize) {
        self.inner
            .lock()
            .expect("SimPlatformStore lock poisoned") // ci-ok: infallible lock
            .pending_spec_publication_postcommit_failures = count;
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
                created_by: "test".to_string(),
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

impl SimPlatformStore {
    #[expect(
        clippy::too_many_arguments,
        reason = "simulation mirrors the atomic platform publication boundary"
    )]
    async fn publish_specs_inner(
        &self,
        tenant: &str,
        specs: &[SpecPublication<'_>],
        mode: SpecPublicationMode,
        constraints: TenantConstraintsPublication<'_>,
        policy: TenantPolicyPublication<'_>,
        os_app: Option<OsAppPublication<'_>>,
        policy_generation: Option<PolicyGenerationPublication<'_>>,
        wasm_modules: &[WasmPublication<'_>],
    ) -> Result<Vec<String>, String> {
        let mut seen_types = BTreeSet::new();
        for spec in specs {
            if !seen_types.insert(spec.entity_type) {
                return Err(format!(
                    "duplicate entity type {} in spec publication for tenant {tenant}",
                    spec.entity_type
                ));
            }
        }
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
        if inner.pending_spec_publication_failures > 0 {
            inner.pending_spec_publication_failures -= 1;
            return Err("SimPlatformStore: injected spec publication failure".into());
        }
        let prob = inner.faults.spec_write_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected spec publication failure".into());
        }
        if matches!(policy, TenantPolicyPublication::Replace(_)) {
            let policy_prob = inner.faults.policy_write_failure_prob;
            if inner.rng.chance(policy_prob) {
                return Err("SimPlatformStore: injected policy publication failure".into());
            }
        }
        if os_app.is_some() {
            let app_prob = inner.faults.app_record_failure_prob;
            if inner.rng.chance(app_prob) {
                return Err("SimPlatformStore: injected app publication failure".into());
            }
        }
        let mut seen_modules = BTreeSet::new();
        for module in wasm_modules {
            if !seen_modules.insert(module.module_name) {
                return Err(format!(
                    "duplicate WASM module {} in spec publication for tenant {tenant}",
                    module.module_name
                ));
            }
        }
        let granular_policy = policy_generation
            .map(|publication| (publication.policy_owner, publication.policy_entries))
            .or_else(|| {
                os_app.and_then(|publication| {
                    publication
                        .policy_owner
                        .map(|owner| (owner, publication.policy_entries))
                })
            });
        if let Some((_, entries)) = granular_policy {
            let mut seen_policy_ids = BTreeSet::new();
            for entry in entries {
                if !seen_policy_ids.insert(entry.policy_id) {
                    return Err(format!(
                        "duplicate Cedar policy {} in spec publication for tenant {tenant}",
                        entry.policy_id
                    ));
                }
            }
        }
        let fail_after_commit = if inner.pending_spec_publication_postcommit_failures > 0 {
            inner.pending_spec_publication_postcommit_failures -= 1;
            true
        } else {
            false
        };
        let incoming_types = specs
            .iter()
            .map(|spec| spec.entity_type)
            .collect::<std::collections::BTreeSet<_>>();
        let removed_entity_types = if mode == SpecPublicationMode::Replace {
            inner
                .specs
                .keys()
                .filter(|(row_tenant, entity_type)| {
                    row_tenant == tenant && !incoming_types.contains(entity_type.as_str())
                })
                .map(|(_, entity_type)| entity_type.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for spec in specs {
            inner.specs.insert(
                (tenant.to_string(), spec.entity_type.to_string()),
                SpecRow {
                    tenant: tenant.to_string(),
                    entity_type: spec.entity_type.to_string(),
                    ioa_source: spec.ioa_source.to_string(),
                    csdl_xml: Some(spec.csdl_xml.to_string()),
                    content_hash: spec.content_hash.to_string(),
                    committed: true,
                },
            );
        }
        for entity_type in &removed_entity_types {
            inner
                .specs
                .remove(&(tenant.to_string(), entity_type.clone()));
        }
        match constraints {
            TenantConstraintsPublication::Preserve => {}
            TenantConstraintsPublication::Replace(Some(source)) => {
                inner
                    .constraints
                    .insert(tenant.to_string(), source.to_string());
            }
            TenantConstraintsPublication::Replace(None) => {
                inner.constraints.remove(tenant);
            }
        }
        if let TenantPolicyPublication::Replace(policy) = policy {
            inner
                .policies
                .insert(tenant.to_string(), policy.to_string());
        }
        if let Some((owner, entries)) = granular_policy {
            inner
                .policy_entries
                .retain(|(row_tenant, _), row| row_tenant != tenant || row.created_by != owner);
            for entry in entries {
                inner.policy_entries.insert(
                    (tenant.to_string(), entry.policy_id.to_string()),
                    PolicyEntryRow {
                        tenant: tenant.to_string(),
                        policy_id: entry.policy_id.to_string(),
                        cedar_text: entry.cedar_text.to_string(),
                        created_by: entry.created_by.to_string(),
                        enabled: true,
                    },
                );
            }
        }
        if let Some(publication) = os_app {
            let key = (tenant.to_string(), publication.record.app_name.clone());
            let now = sim_now().to_rfc3339();
            let mut record = publication.record.clone();
            record.installed_at = inner
                .installed_app_records
                .get(&key)
                .and_then(|existing| existing.installed_at.clone())
                .or_else(|| Some(now.clone()));
            record.last_reconciled_at = Some(now);
            inner.installed_apps.insert(key.clone());
            inner.installed_app_records.insert(key, record);
        }
        for module in wasm_modules {
            let key = (tenant.to_string(), module.module_name.to_string());
            let replace_uploaded_wasm = module.source == "bundled-replace-upload";
            let persisted_source = if replace_uploaded_wasm {
                "bundled"
            } else {
                module.source
            };
            let should_replace = inner.wasm_modules.get(&key).is_none_or(|existing| {
                existing.sha256_hash != module.sha256_hash
                    && (replace_uploaded_wasm
                        || persisted_source == "upload"
                        || existing.source == "bundled")
            });
            if should_replace {
                inner.wasm_modules.insert(
                    key,
                    WasmModuleRow {
                        tenant: tenant.to_string(),
                        module_name: module.module_name.to_string(),
                        wasm_bytes: module.wasm_bytes.to_vec(),
                        sha256_hash: module.sha256_hash.to_string(),
                        source: persisted_source.to_string(),
                    },
                );
            }
        }
        if fail_after_commit {
            Err("SimPlatformStore: injected post-commit publication failure".into())
        } else {
            Ok(removed_entity_types)
        }
    }
}
