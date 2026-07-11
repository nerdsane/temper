//! Platform-level storage abstraction for DST (deterministic simulation testing).
//!
//! [`PlatformStore`] abstracts the ~12 platform storage methods used by
//! `install_os_app`, bootstrap, and the verification cascade. The production
//! implementation delegates to [`TursoEventStore`]; the simulation implementation
//! ([`SimPlatformStore`], behind `#[cfg(feature = "sim")]`) uses in-memory
//! `BTreeMap` storage with fault injection for deterministic testing.

use std::collections::{BTreeMap, BTreeSet};

mod source_snapshot;
pub use source_snapshot::RegistrySourceSnapshot;

/// Maximum UTF-8 byte length of one persisted quarantine diagnostic.
pub(crate) const REGISTRY_QUARANTINE_DETAIL_BUDGET_BYTES: usize = 512;

// ---------------------------------------------------------------------------
// Row / update types
// ---------------------------------------------------------------------------

/// Row returned by [`PlatformStore::load_specs()`].
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Monotonic persisted source version.
    pub version: i64,
    /// Whether this spec has been committed (WAL-style commit flag).
    pub committed: bool,
    /// Persisted verification status.
    pub verification_status: String,
    /// Whether the persisted source passed verification.
    pub verified: bool,
    /// Number of completed verification levels.
    pub levels_passed: Option<i32>,
    /// Number of verification levels attempted.
    pub levels_total: Option<i32>,
    /// Serialized verification result, when available.
    pub verification_result: Option<String>,
    /// Backend-normalized last-update timestamp.
    pub updated_at: String,
}

/// Versioned tenant-level cross-entity constraint source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantConstraintRow {
    /// Tenant owning the constraint source.
    pub tenant: String,
    /// Raw `cross-invariants.toml` source.
    pub source: String,
    /// Monotonic persisted version used by repair compare-and-set.
    pub version: i64,
}

/// Durable active registry-restore quarantine record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RegistryQuarantineRecord {
    /// Tenant owning the persisted spec.
    pub tenant: String,
    /// Entity type withheld from activation.
    pub entity_type: String,
    /// Persisted source version that failed.
    pub spec_version: i64,
    /// Constraint source version compiled with the spec, or `None` if absent.
    pub constraint_version: Option<i64>,
    /// Stable failure category.
    pub reason: String,
    /// Source document category.
    pub source_kind: String,
    /// One-based source line, when available.
    pub source_line: Option<i64>,
    /// One-based source column, when available.
    pub source_column: Option<i64>,
    /// Bounded parser/registration diagnostic.
    pub detail: String,
    /// Timestamp of operator acknowledgment, when acknowledged.
    pub acknowledged_at: Option<String>,
    /// Timestamp when this version first failed.
    pub created_at: String,
    /// Timestamp when this version most recently failed.
    pub last_observed_at: String,
}

/// Borrowed quarantine payload used while reconciling a restore attempt.
#[derive(Debug, Clone, Copy)]
pub struct RegistryQuarantineUpsert<'a> {
    /// Tenant owning the persisted spec.
    pub tenant: &'a str,
    /// Entity type withheld from activation.
    pub entity_type: &'a str,
    /// Persisted source version that failed.
    pub spec_version: i64,
    /// Constraint source version compiled with the spec, or `None` if absent.
    pub constraint_version: Option<i64>,
    /// Stable failure category.
    pub reason: &'a str,
    /// Source document category.
    pub source_kind: &'a str,
    /// One-based source line, when available.
    pub source_line: Option<i64>,
    /// One-based source column, when available.
    pub source_column: Option<i64>,
    /// Bounded parser/registration diagnostic.
    pub detail: &'a str,
}

fn validate_registry_quarantine_snapshot(
    active: &[RegistryQuarantineUpsert<'_>],
) -> Result<(), String> {
    let mut active_entities = BTreeSet::new();
    for row in active {
        if row.spec_version <= 0 {
            return Err("registry quarantine spec version must be positive".to_string());
        }
        if !matches!(
            row.reason,
            "missing_csdl" | "invalid_csdl" | "registration_failed"
        ) {
            return Err(format!(
                "invalid registry quarantine reason '{}'",
                row.reason
            ));
        }
        if !matches!(
            row.source_kind,
            "csdl" | "ioa" | "cross_invariants" | "registration"
        ) {
            return Err(format!(
                "invalid registry quarantine source kind '{}'",
                row.source_kind
            ));
        }
        if row.detail.len() > REGISTRY_QUARANTINE_DETAIL_BUDGET_BYTES {
            return Err(format!(
                "registry quarantine detail exceeds {REGISTRY_QUARANTINE_DETAIL_BUDGET_BYTES}-byte budget"
            ));
        }
        if row.constraint_version.is_some_and(|version| version <= 0) {
            return Err("registry quarantine constraint version must be positive".to_string());
        }
        if !active_entities.insert((row.tenant, row.entity_type)) {
            return Err(format!(
                "multiple active registry quarantine versions for tenant '{}', entity '{}'",
                row.tenant, row.entity_type
            ));
        }
    }
    Ok(())
}

/// One exact active-record resolution paired with its validated repair version.
#[derive(Debug, Clone, Copy)]
pub struct RegistryQuarantineResolution<'a> {
    /// Tenant owning both records.
    pub tenant: &'a str,
    /// Entity type being reactivated.
    pub entity_type: &'a str,
    /// Active quarantine version to resolve.
    pub quarantined_version: i64,
    /// Constraint version recorded on the active quarantine, or `None` if absent.
    pub quarantined_constraint_version: Option<i64>,
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
    /// Provenance: `"bundled"` (os-apps install pipeline) or `"upload"` (hot
    /// upload via `POST /api/wasm/modules/{name}`). The install pipeline must
    /// not clobber rows whose source is `"upload"`.
    pub source: String,
}

/// One granular Cedar policy row.
#[derive(Debug, Clone)]
pub struct PolicyEntryRow {
    pub tenant: String,
    pub policy_id: String,
    pub cedar_text: String,
    pub enabled: bool,
}

/// Durable metadata for an installed OS app bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstalledAppRecord {
    pub tenant: String,
    pub app_name: String,
    pub source_kind: String,
    pub app_ref: String,
    pub version_hash: String,
    pub pinned_version_hash: String,
    pub current_version_hash: String,
    pub follow_policy: String,
    pub closure_id: String,
    pub registry_url: String,
    pub registry_tenant: String,
    pub app_version: String,
    pub bundle_digest: String,
    pub spec_digest: String,
    pub policy_digest: String,
    pub wasm_digest: String,
    pub content_digest: String,
    pub seed_digest: String,
    pub installed_at: Option<String>,
    pub last_reconciled_at: Option<String>,
    pub status: String,
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

    /// Load persisted cross-entity constraints keyed by tenant.
    async fn load_tenant_constraints(&self) -> Result<Vec<TenantConstraintRow>, String>;

    /// Delete a spec for a given tenant/entity_type.
    ///
    /// Used only for explicit rollback when `install_os_app` fails mid-write.
    /// Restore failures retain committed rows as quarantined evidence.
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

    /// Replace the active quarantine snapshot while retaining resolved history.
    async fn replace_registry_restore_quarantines(
        &self,
        source: &RegistrySourceSnapshot,
        active: &[RegistryQuarantineUpsert<'_>],
    ) -> Result<bool, String>;

    /// Replace one tenant's active snapshot without changing other tenants.
    async fn replace_registry_restore_quarantines_for_tenant(
        &self,
        tenant: &str,
        source: &RegistrySourceSnapshot,
        active: &[RegistryQuarantineUpsert<'_>],
    ) -> Result<bool, String>;

    /// Atomically resolve active quarantines after their repair versions validate.
    async fn resolve_registry_restore_quarantines(
        &self,
        source: &RegistrySourceSnapshot,
        resolutions: &[RegistryQuarantineResolution<'_>],
    ) -> Result<bool, String>;

    /// Mark an active quarantine acknowledged without hiding it.
    /// Acknowledge exactly the version the operator inspected and return the
    /// current active identity. A different returned identity is a stale-write
    /// conflict; `None` means the entity has no active quarantine.
    async fn acknowledge_registry_restore_quarantine(
        &self,
        tenant: &str,
        entity_type: &str,
        spec_version: i64,
        constraint_version: Option<i64>,
    ) -> Result<Option<(i64, Option<i64>)>, String>;

    /// Load active quarantine records in deterministic order.
    async fn load_registry_restore_quarantines(
        &self,
    ) -> Result<Vec<RegistryQuarantineRecord>, String>;

    /// Load at most `limit` active records for one tenant.
    async fn load_registry_restore_quarantines_for_tenant(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<RegistryQuarantineRecord>, String>;

    // ── Cedar policies ───────────────────────────────────────────────

    /// Upsert Cedar policy text for a tenant.
    async fn upsert_tenant_policy(&self, tenant: &str, policy_text: &str) -> Result<(), String>;

    /// Upsert tenant-level cross-entity constraint definitions.
    async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), String>;

    /// Load all tenant Cedar policies.
    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String>;

    /// Load all granular Cedar policy rows.
    async fn load_policy_entries(&self) -> Result<Vec<PolicyEntryRow>, String>;

    // ── Installed apps ───────────────────────────────────────────────

    /// Check if an OS app is already installed for a tenant.
    async fn is_app_installed(&self, tenant: &str, app_name: &str) -> Result<bool, String>;

    /// Record that an OS app was installed in a tenant.
    async fn record_installed_app(&self, tenant: &str, app_name: &str) -> Result<(), String>;

    /// Record digest metadata for an installed OS app bundle.
    async fn record_installed_app_metadata(
        &self,
        record: &InstalledAppRecord,
    ) -> Result<(), String>;

    /// Load digest metadata for an installed OS app bundle.
    async fn get_installed_app(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<InstalledAppRecord>, String>;

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
    ///
    /// `source` distinguishes the os-apps install pipeline (`"bundled"`) from
    /// the hot-upload API (`"upload"`). Plain bundled installs preserve existing
    /// upload rows, while OS-app reconcile can explicitly replace stale uploads
    /// when the installed app's bundled WASM digest changes.
    async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
        source: &str,
    ) -> Result<(), String>;
}

mod postgres;
mod turso;

#[cfg(feature = "sim")]
mod sim;
#[cfg(feature = "sim")]
pub use sim::*;
