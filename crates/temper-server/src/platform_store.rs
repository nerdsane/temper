//! Platform-level storage abstraction for DST (deterministic simulation testing).
//!
//! [`PlatformStore`] abstracts the ~12 platform storage methods used by
//! `install_skill`, bootstrap, and the verification cascade. The production
//! implementation delegates to [`TursoEventStore`]; the simulation implementation
//! ([`SimPlatformStore`], behind `#[cfg(feature = "sim")]`) uses in-memory
//! `BTreeMap` storage with fault injection for deterministic testing.

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Row / update types
// ---------------------------------------------------------------------------

/// Row returned by [`PlatformStore::load_specs()`].
#[derive(Debug, Clone)]
pub struct SpecRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// IOA TOML source.
    pub ioa_source: String,
    /// CSDL XML (may be absent for old rows).
    pub csdl_xml: Option<String>,
    /// SHA-256 hex digest of the IOA source content.
    pub content_hash: String,
    /// Whether this spec has been committed (WAL-style commit flag).
    pub committed: bool,
}

/// Update payload for [`PlatformStore::persist_spec_verification()`].
#[derive(Debug, Clone)]
pub struct SpecVerificationUpdate<'a> {
    /// Verification status string (pending/running/passed/failed/partial).
    pub status: &'a str,
    /// Whether the spec has been verified.
    pub verified: bool,
    /// Number of verification levels that passed.
    pub levels_passed: Option<i32>,
    /// Total number of verification levels.
    pub levels_total: Option<i32>,
    /// Serialized verification result JSON.
    pub verification_result_json: Option<&'a str>,
}

/// WASM module row returned by [`PlatformStore`] WASM queries.
#[derive(Debug, Clone)]
pub struct WasmModuleRow {
    /// Tenant name.
    pub tenant: String,
    /// Module name.
    pub module_name: String,
    /// Raw WASM binary.
    pub wasm_bytes: Vec<u8>,
    /// SHA-256 hash of the WASM binary.
    pub sha256_hash: String,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Platform-level storage abstraction.
///
/// Covers spec persistence, Cedar policies, installed apps, pending decisions,
/// and WASM modules. Production uses [`TursoEventStore`]; simulation uses
/// [`SimPlatformStore`].
#[async_trait::async_trait]
pub trait PlatformStore: Send + Sync {
    // ── Spec persistence ─────────────────────────────────────────────

    /// Upsert a spec source (IOA + CSDL) for a tenant/entity_type.
    async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), String>;

    /// Load all persisted specs (for startup recovery).
    async fn load_specs(&self) -> Result<Vec<SpecRow>, String>;

    /// Delete a spec for a given tenant/entity_type.
    ///
    /// Used for cleanup when `install_skill` fails mid-write (atomicity)
    /// and for reconciliation during `restore_registry_from_platform_store`.
    async fn delete_spec(&self, tenant: &str, entity_type: &str) -> Result<(), String>;

    /// Mark all uncommitted specs for a tenant as committed.
    async fn commit_specs(&self, tenant: &str) -> Result<(), String>;
    /// Delete all uncommitted specs across all tenants.
    async fn delete_uncommitted_specs(&self) -> Result<usize, String>;

    /// Load verification cache: (entity_type -> (content_hash, verified)) for a tenant.
    async fn load_verification_cache(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, (String, bool)>, String>;

    /// Persist verification result for a spec.
    async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        update: SpecVerificationUpdate<'_>,
    ) -> Result<(), String>;

    // ── Cedar policies ───────────────────────────────────────────────

    /// Upsert Cedar policy text for a tenant.
    async fn upsert_tenant_policy(&self, tenant: &str, policy_text: &str) -> Result<(), String>;

    /// Load all tenant Cedar policies.
    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String>;

    // ── Installed apps ───────────────────────────────────────────────

    /// Check if an OS app is already installed for a tenant.
    async fn is_app_installed(&self, tenant: &str, app_name: &str) -> Result<bool, String>;

    /// Record that an OS app was installed in a tenant.
    async fn record_installed_app(&self, tenant: &str, app_name: &str) -> Result<(), String>;

    /// List all installed apps across all tenants (for boot + UI).
    async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, String>;

    // ── Pending decisions ────────────────────────────────────────────

    /// Upsert a pending decision (insert or update).
    async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data: &str,
    ) -> Result<(), String>;

    /// Load all pending decisions (newest first, up to `limit`).
    async fn load_pending_decisions(&self, limit: usize) -> Result<Vec<String>, String>;

    // ── WASM modules ─────────────────────────────────────────────────

    /// Load all WASM modules for a tenant.
    async fn load_all_wasm_modules(&self, tenant: &str) -> Result<Vec<WasmModuleRow>, String>;

    /// Load all WASM modules across all tenants (for startup recovery).
    async fn load_wasm_modules_all_tenants(&self) -> Result<Vec<WasmModuleRow>, String>;

    /// Upsert a WASM module binary for a tenant.
    async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
    ) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// TursoEventStore implementation
// ---------------------------------------------------------------------------

use temper_store_turso::{TursoEventStore, TursoSpecVerificationUpdate};

#[async_trait::async_trait]
impl PlatformStore for TursoEventStore {
    async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), String> {
        self.upsert_spec(tenant, entity_type, ioa_source, csdl_xml, content_hash)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_specs(&self) -> Result<Vec<SpecRow>, String> {
        let rows = self.load_specs().await.map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| SpecRow {
                tenant: r.tenant,
                entity_type: r.entity_type,
                ioa_source: r.ioa_source,
                csdl_xml: r.csdl_xml,
                content_hash: r.content_hash.unwrap_or_default(),
                committed: r.committed,
            })
            .collect())
    }

    async fn delete_spec(&self, tenant: &str, entity_type: &str) -> Result<(), String> {
        self.delete_spec(tenant, entity_type)
            .await
            .map_err(|e| e.to_string())
    }

    async fn commit_specs(&self, tenant: &str) -> Result<(), String> {
        self.commit_specs(tenant).await.map_err(|e| e.to_string())
    }
    async fn delete_uncommitted_specs(&self) -> Result<usize, String> {
        self.delete_uncommitted_specs()
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_verification_cache(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, (String, bool)>, String> {
        self.load_verification_cache(tenant)
            .await
            .map_err(|e| e.to_string())
    }

    async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        update: SpecVerificationUpdate<'_>,
    ) -> Result<(), String> {
        let turso_update = TursoSpecVerificationUpdate {
            status: update.status,
            verified: update.verified,
            levels_passed: update.levels_passed,
            levels_total: update.levels_total,
            verification_result_json: update.verification_result_json,
        };
        self.persist_spec_verification(tenant, entity_type, turso_update)
            .await
            .map_err(|e| e.to_string())
    }

    async fn upsert_tenant_policy(&self, tenant: &str, policy_text: &str) -> Result<(), String> {
        self.upsert_tenant_policy(tenant, policy_text)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String> {
        self.load_tenant_policies().await.map_err(|e| e.to_string())
    }

    async fn is_app_installed(&self, tenant: &str, app_name: &str) -> Result<bool, String> {
        self.is_app_installed(tenant, app_name)
            .await
            .map_err(|e| e.to_string())
    }

    async fn record_installed_app(&self, tenant: &str, app_name: &str) -> Result<(), String> {
        self.record_installed_app(tenant, app_name)
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, String> {
        self.list_all_installed_apps()
            .await
            .map_err(|e| e.to_string())
    }

    async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data: &str,
    ) -> Result<(), String> {
        self.upsert_pending_decision(id, tenant, status, data)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_pending_decisions(&self, limit: usize) -> Result<Vec<String>, String> {
        self.load_pending_decisions(limit as i64)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_all_wasm_modules(&self, tenant: &str) -> Result<Vec<WasmModuleRow>, String> {
        let rows = self
            .load_all_wasm_modules(tenant)
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| WasmModuleRow {
                tenant: r.tenant,
                module_name: r.module_name,
                wasm_bytes: r.wasm_bytes,
                sha256_hash: r.sha256_hash,
            })
            .collect())
    }

    async fn load_wasm_modules_all_tenants(&self) -> Result<Vec<WasmModuleRow>, String> {
        let rows = self
            .load_wasm_modules_all_tenants()
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| WasmModuleRow {
                tenant: r.tenant,
                module_name: r.module_name,
                wasm_bytes: r.wasm_bytes,
                sha256_hash: r.sha256_hash,
            })
            .collect())
    }

    async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
    ) -> Result<(), String> {
        self.upsert_wasm_module(tenant, name, bytes, hash)
            .await
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// SimPlatformStore (behind cfg(feature = "sim"))
// ---------------------------------------------------------------------------

#[cfg(feature = "sim")]
pub use sim_platform_store::*;

#[cfg(feature = "sim")]
mod sim_platform_store {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};
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
        /// Specs keyed by (tenant, entity_type).
        specs: BTreeMap<(String, String), SpecRow>,
        /// Verification cache: (tenant, entity_type) -> (content_hash, verified).
        verification_cache: BTreeMap<(String, String), (String, bool)>,
        /// Cedar policies keyed by tenant.
        policies: BTreeMap<String, String>,
        /// Installed apps: (tenant, app_name).
        installed_apps: BTreeSet<(String, String)>,
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
                    specs: BTreeMap::new(),
                    verification_cache: BTreeMap::new(),
                    policies: BTreeMap::new(),
                    installed_apps: BTreeSet::new(),
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
    }

    impl std::fmt::Debug for SimPlatformStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
            f.debug_struct("SimPlatformStore")
                .field("specs", &inner.specs.len())
                .field("policies", &inner.policies.len())
                .field("installed_apps", &inner.installed_apps.len())
                .field("wasm_modules", &inner.wasm_modules.len())
                .finish()
        }
    }

    #[async_trait::async_trait]
    impl PlatformStore for SimPlatformStore {
        async fn upsert_spec(
            &self,
            tenant: &str,
            entity_type: &str,
            ioa_source: &str,
            csdl_xml: &str,
            content_hash: &str,
        ) -> Result<(), String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.spec_write_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected spec write failure".into());
            }

            let key = (tenant.to_string(), entity_type.to_string());
            inner.specs.insert(
                key,
                SpecRow {
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    ioa_source: ioa_source.to_string(),
                    csdl_xml: Some(csdl_xml.to_string()),
                    content_hash: content_hash.to_string(),
                    committed: false,
                },
            );
            Ok(())
        }

        async fn load_specs(&self) -> Result<Vec<SpecRow>, String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.spec_read_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected spec read failure".into());
            }

            Ok(inner
                .specs
                .values()
                .filter(|s| s.committed)
                .cloned()
                .collect())
        }

        async fn delete_spec(&self, tenant: &str, entity_type: &str) -> Result<(), String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
            let prob = inner.faults.cleanup_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected cleanup failure".into());
            }
            inner
                .specs
                .remove(&(tenant.to_string(), entity_type.to_string()));
            Ok(())
        }

        async fn commit_specs(&self, tenant: &str) -> Result<(), String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
            for spec in inner.specs.values_mut() {
                if spec.tenant == tenant {
                    spec.committed = true;
                }
            }
            Ok(())
        }

        async fn delete_uncommitted_specs(&self) -> Result<usize, String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
            let before = inner.specs.len();
            inner.specs.retain(|_, s| s.committed);
            Ok(before - inner.specs.len())
        }

        async fn load_verification_cache(
            &self,
            tenant: &str,
        ) -> Result<BTreeMap<String, (String, bool)>, String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.spec_read_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected verification cache read failure".into());
            }

            let mut cache = BTreeMap::new();
            for ((t, et), (hash, verified)) in &inner.verification_cache {
                if t == tenant {
                    cache.insert(et.clone(), (hash.clone(), *verified));
                }
            }
            Ok(cache)
        }

        async fn persist_spec_verification(
            &self,
            tenant: &str,
            entity_type: &str,
            update: SpecVerificationUpdate<'_>,
        ) -> Result<(), String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.spec_write_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected verification write failure".into());
            }

            let key = (tenant.to_string(), entity_type.to_string());
            inner
                .verification_cache
                .insert(key, (update.status.to_string(), update.verified));
            Ok(())
        }

        async fn upsert_tenant_policy(
            &self,
            tenant: &str,
            policy_text: &str,
        ) -> Result<(), String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.policy_write_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected policy write failure".into());
            }

            inner
                .policies
                .insert(tenant.to_string(), policy_text.to_string());
            Ok(())
        }

        async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.policy_read_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected policy read failure".into());
            }

            Ok(inner
                .policies
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect())
        }

        async fn is_app_installed(&self, tenant: &str, app_name: &str) -> Result<bool, String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.app_list_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected app query failure".into());
            }

            Ok(inner
                .installed_apps
                .contains(&(tenant.to_string(), app_name.to_string())))
        }

        async fn record_installed_app(&self, tenant: &str, app_name: &str) -> Result<(), String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.app_record_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected app record failure".into());
            }

            inner
                .installed_apps
                .insert((tenant.to_string(), app_name.to_string()));
            Ok(())
        }

        async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.app_list_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected app list failure".into());
            }

            Ok(inner.installed_apps.iter().cloned().collect())
        }

        async fn upsert_pending_decision(
            &self,
            id: &str,
            tenant: &str,
            status: &str,
            data: &str,
        ) -> Result<(), String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.decision_write_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected decision write failure".into());
            }

            inner.pending_decisions.insert(
                id.to_string(),
                (tenant.to_string(), status.to_string(), data.to_string()),
            );
            Ok(())
        }

        async fn load_pending_decisions(&self, limit: usize) -> Result<Vec<String>, String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.decision_read_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected decision read failure".into());
            }

            Ok(inner
                .pending_decisions
                .values()
                .rev()
                .take(limit)
                .map(|(_, _, data)| data.clone())
                .collect())
        }

        async fn load_all_wasm_modules(&self, tenant: &str) -> Result<Vec<WasmModuleRow>, String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.wasm_read_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected WASM read failure".into());
            }

            Ok(inner
                .wasm_modules
                .values()
                .filter(|m| m.tenant == tenant)
                .cloned()
                .collect())
        }

        async fn load_wasm_modules_all_tenants(&self) -> Result<Vec<WasmModuleRow>, String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.wasm_read_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected WASM read failure".into());
            }

            Ok(inner.wasm_modules.values().cloned().collect())
        }

        async fn upsert_wasm_module(
            &self,
            tenant: &str,
            name: &str,
            bytes: &[u8],
            hash: &str,
        ) -> Result<(), String> {
            let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

            let prob = inner.faults.spec_write_failure_prob;
            if inner.rng.chance(prob) {
                return Err("SimPlatformStore: injected WASM write failure".into());
            }

            let key = (tenant.to_string(), name.to_string());
            inner.wasm_modules.insert(
                key,
                WasmModuleRow {
                    tenant: tenant.to_string(),
                    module_name: name.to_string(),
                    wasm_bytes: bytes.to_vec(),
                    sha256_hash: hash.to_string(),
                },
            );
            Ok(())
        }
    }
}
