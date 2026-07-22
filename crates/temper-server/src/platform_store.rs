//! Platform-level storage abstraction for DST (deterministic simulation testing).
//!
//! [`PlatformStore`] abstracts the ~12 platform storage methods used by
//! `install_os_app`, bootstrap, and the verification cascade. The production
//! implementation delegates to [`TursoEventStore`]; the simulation implementation
//! ([`SimPlatformStore`], behind `#[cfg(feature = "sim")]`) uses in-memory
//! `BTreeMap` storage with fault injection for deterministic testing.

use std::collections::BTreeMap;

#[cfg(feature = "sim")]
mod sim;

#[cfg(feature = "sim")]
pub use sim::*;

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

/// One spec row in an atomic durable publication.
#[derive(Debug, Clone, Copy)]
pub struct SpecPublication<'a> {
    /// Entity type owning this specification.
    pub entity_type: &'a str,
    /// IOA source published for the entity type.
    pub ioa_source: &'a str,
    /// Tenant CSDL generation paired with this publication.
    pub csdl_xml: &'a str,
    /// Stable digest of the IOA source.
    pub content_hash: &'a str,
}

/// Durable omission semantics for an atomic spec publication.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SpecPublicationMode {
    /// Upsert submitted rows and preserve every omitted durable spec.
    Merge,
    /// Make the submitted entity types the tenant's complete durable set.
    Replace,
}

/// Tenant-constraint mutation included in an atomic spec publication.
#[derive(Debug, Clone, Copy)]
pub enum TenantConstraintsPublication<'a> {
    /// Leave the durable tenant constraint row unchanged.
    Preserve,
    /// Replace the row, or delete it when the payload is `None`.
    Replace(Option<&'a str>),
}

/// Tenant Cedar-policy mutation included in an atomic spec publication.
#[derive(Debug, Clone, Copy)]
pub enum TenantPolicyPublication<'a> {
    /// Leave the durable tenant policy unchanged.
    Preserve,
    /// Replace the durable tenant policy with this complete policy set.
    Replace(&'a str),
}

/// OS-app metadata committed in the same transaction as its spec generation.
#[derive(Debug, Clone, Copy)]
pub struct OsAppPublication<'a> {
    /// Complete target digest metadata and in-progress publication context.
    pub record: &'a InstalledAppRecord,
    /// Policy owner whose complete row set is replaced by this app generation.
    pub policy_owner: Option<&'a str>,
    /// Granular policy rows committed beside the aggregate tenant policy.
    pub policy_entries: &'a [PolicyEntryPublication<'a>],
}

/// Complete granular Cedar row generation replaced atomically with specs.
#[derive(Debug, Clone, Copy)]
pub struct PolicyGenerationPublication<'a> {
    /// Stable owner whose prior row set is replaced.
    pub policy_owner: &'a str,
    /// Complete next row set owned by `policy_owner`.
    pub policy_entries: &'a [PolicyEntryPublication<'a>],
}

/// One granular Cedar policy row in an atomic OS-app publication.
#[derive(Debug, Clone, Copy)]
pub struct PolicyEntryPublication<'a> {
    pub policy_id: &'a str,
    pub cedar_text: &'a str,
    pub created_by: &'a str,
}

/// WASM module bytes committed in the same durable generation as specs.
#[derive(Debug, Clone, Copy)]
pub struct WasmPublication<'a> {
    /// Tenant-local module name.
    pub module_name: &'a str,
    /// Validated module bytes.
    pub wasm_bytes: &'a [u8],
    /// SHA-256 digest of `wasm_bytes`.
    pub sha256_hash: &'a str,
    /// Durable provenance (`upload` or `bundled`).
    pub source: &'a str,
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
    pub created_by: String,
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

    /// Atomically publish spec rows and return every durable type removed by a
    /// replace-mode registration.
    #[expect(
        clippy::too_many_arguments,
        reason = "atomic publication carries every durable tenant-generation component"
    )]
    async fn publish_specs(
        &self,
        tenant: &str,
        specs: &[SpecPublication<'_>],
        mode: SpecPublicationMode,
        constraints: TenantConstraintsPublication<'_>,
        policy: TenantPolicyPublication<'_>,
        os_app: Option<OsAppPublication<'_>>,
        policy_generation: Option<PolicyGenerationPublication<'_>>,
        wasm_modules: &[WasmPublication<'_>],
    ) -> Result<Vec<String>, String>;

    /// Load all persisted specs (for startup recovery).
    async fn load_specs(&self) -> Result<Vec<SpecRow>, String>;

    /// Delete a spec for a given tenant/entity_type.
    ///
    /// Used for cleanup when `install_os_app` fails mid-write (atomicity)
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

// ---------------------------------------------------------------------------
// TursoEventStore implementation
// ---------------------------------------------------------------------------

use temper_store_postgres::{
    PostgresEventStore, PostgresInstalledAppRow, PostgresSpecVerificationUpdate,
};
use temper_store_turso::{TursoEventStore, TursoInstalledAppRow, TursoSpecVerificationUpdate};

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

    async fn publish_specs(
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
        let rows = specs
            .iter()
            .map(|spec| {
                (
                    spec.entity_type,
                    spec.ioa_source,
                    spec.csdl_xml,
                    spec.content_hash,
                )
            })
            .collect::<Vec<_>>();
        let constraints = match constraints {
            TenantConstraintsPublication::Preserve => None,
            TenantConstraintsPublication::Replace(source) => Some(source),
        };
        let policy = match policy {
            TenantPolicyPublication::Preserve => None,
            TenantPolicyPublication::Replace(source) => Some(source),
        };
        let granular_policy = policy_generation
            .map(|publication| (publication.policy_owner, publication.policy_entries))
            .or_else(|| {
                os_app.and_then(|publication| {
                    publication
                        .policy_owner
                        .map(|owner| (owner, publication.policy_entries))
                })
            });
        let policy_owner = granular_policy.map(|(owner, _)| owner);
        let policy_entries = granular_policy
            .map(|(_, entries)| {
                entries
                    .iter()
                    .map(|entry| (entry.policy_id, entry.cedar_text, entry.created_by))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let os_app = os_app.map(|publication| TursoInstalledAppRow {
            tenant_id: publication.record.tenant.clone(),
            app_name: publication.record.app_name.clone(),
            source_kind: publication.record.source_kind.clone(),
            app_ref: publication.record.app_ref.clone(),
            version_hash: publication.record.version_hash.clone(),
            pinned_version_hash: publication.record.pinned_version_hash.clone(),
            current_version_hash: publication.record.current_version_hash.clone(),
            follow_policy: publication.record.follow_policy.clone(),
            closure_id: publication.record.closure_id.clone(),
            registry_url: publication.record.registry_url.clone(),
            registry_tenant: publication.record.registry_tenant.clone(),
            app_version: publication.record.app_version.clone(),
            bundle_digest: publication.record.bundle_digest.clone(),
            spec_digest: publication.record.spec_digest.clone(),
            policy_digest: publication.record.policy_digest.clone(),
            wasm_digest: publication.record.wasm_digest.clone(),
            content_digest: publication.record.content_digest.clone(),
            seed_digest: publication.record.seed_digest.clone(),
            installed_at: publication.record.installed_at.clone().unwrap_or_default(),
            last_reconciled_at: publication.record.last_reconciled_at.clone(),
            status: publication.record.status.clone(),
        });
        let wasm_rows = wasm_modules
            .iter()
            .map(|module| {
                (
                    module.module_name,
                    module.wasm_bytes,
                    module.sha256_hash,
                    module.source,
                )
            })
            .collect::<Vec<_>>();
        TursoEventStore::publish_specs(
            self,
            tenant,
            &rows,
            mode == SpecPublicationMode::Replace,
            constraints,
            policy,
            os_app.as_ref(),
            &wasm_rows,
            policy_owner,
            &policy_entries,
        )
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

    async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), String> {
        self.upsert_tenant_constraints(tenant, cross_invariants_toml)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String> {
        self.load_tenant_policies().await.map_err(|e| e.to_string())
    }

    async fn load_policy_entries(&self) -> Result<Vec<PolicyEntryRow>, String> {
        let rows = self.load_all_policies().await.map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|row| PolicyEntryRow {
                tenant: row.tenant,
                policy_id: row.policy_id,
                cedar_text: row.cedar_text,
                created_by: row.created_by,
                enabled: row.enabled,
            })
            .collect())
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

    async fn record_installed_app_metadata(
        &self,
        record: &InstalledAppRecord,
    ) -> Result<(), String> {
        let row = TursoInstalledAppRow {
            tenant_id: record.tenant.clone(),
            app_name: record.app_name.clone(),
            source_kind: record.source_kind.clone(),
            app_ref: record.app_ref.clone(),
            version_hash: record.version_hash.clone(),
            pinned_version_hash: record.pinned_version_hash.clone(),
            current_version_hash: record.current_version_hash.clone(),
            follow_policy: record.follow_policy.clone(),
            closure_id: record.closure_id.clone(),
            registry_url: record.registry_url.clone(),
            registry_tenant: record.registry_tenant.clone(),
            app_version: record.app_version.clone(),
            bundle_digest: record.bundle_digest.clone(),
            spec_digest: record.spec_digest.clone(),
            policy_digest: record.policy_digest.clone(),
            wasm_digest: record.wasm_digest.clone(),
            content_digest: record.content_digest.clone(),
            seed_digest: record.seed_digest.clone(),
            installed_at: record.installed_at.clone().unwrap_or_default(),
            last_reconciled_at: record.last_reconciled_at.clone(),
            status: record.status.clone(),
        };
        self.record_installed_app_metadata(&row)
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_installed_app(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<InstalledAppRecord>, String> {
        self.get_installed_app(tenant, app_name)
            .await
            .map(|row| {
                row.map(|row| InstalledAppRecord {
                    tenant: row.tenant_id,
                    app_name: row.app_name,
                    source_kind: row.source_kind,
                    app_ref: row.app_ref,
                    version_hash: row.version_hash,
                    pinned_version_hash: row.pinned_version_hash,
                    current_version_hash: row.current_version_hash,
                    follow_policy: row.follow_policy,
                    closure_id: row.closure_id,
                    registry_url: row.registry_url,
                    registry_tenant: row.registry_tenant,
                    app_version: row.app_version,
                    bundle_digest: row.bundle_digest,
                    spec_digest: row.spec_digest,
                    policy_digest: row.policy_digest,
                    wasm_digest: row.wasm_digest,
                    content_digest: row.content_digest,
                    seed_digest: row.seed_digest,
                    installed_at: Some(row.installed_at),
                    last_reconciled_at: row.last_reconciled_at,
                    status: row.status,
                })
            })
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
                source: r.source,
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
                source: r.source,
            })
            .collect())
    }

    async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
        source: &str,
    ) -> Result<(), String> {
        self.upsert_wasm_module(tenant, name, bytes, hash, source)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl PlatformStore for PostgresEventStore {
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

    async fn publish_specs(
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
        let rows = specs
            .iter()
            .map(|spec| {
                (
                    spec.entity_type,
                    spec.ioa_source,
                    spec.csdl_xml,
                    spec.content_hash,
                )
            })
            .collect::<Vec<_>>();
        let constraints = match constraints {
            TenantConstraintsPublication::Preserve => None,
            TenantConstraintsPublication::Replace(source) => Some(source),
        };
        let policy = match policy {
            TenantPolicyPublication::Preserve => None,
            TenantPolicyPublication::Replace(source) => Some(source),
        };
        let granular_policy = policy_generation
            .map(|publication| (publication.policy_owner, publication.policy_entries))
            .or_else(|| {
                os_app.and_then(|publication| {
                    publication
                        .policy_owner
                        .map(|owner| (owner, publication.policy_entries))
                })
            });
        let policy_owner = granular_policy.map(|(owner, _)| owner);
        let policy_entries = granular_policy
            .map(|(_, entries)| {
                entries
                    .iter()
                    .map(|entry| (entry.policy_id, entry.cedar_text, entry.created_by))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let os_app = os_app.map(|publication| PostgresInstalledAppRow {
            tenant: publication.record.tenant.clone(),
            app_name: publication.record.app_name.clone(),
            source_kind: publication.record.source_kind.clone(),
            app_ref: publication.record.app_ref.clone(),
            version_hash: publication.record.version_hash.clone(),
            pinned_version_hash: publication.record.pinned_version_hash.clone(),
            current_version_hash: publication.record.current_version_hash.clone(),
            follow_policy: publication.record.follow_policy.clone(),
            closure_id: publication.record.closure_id.clone(),
            registry_url: publication.record.registry_url.clone(),
            registry_tenant: publication.record.registry_tenant.clone(),
            app_version: publication.record.app_version.clone(),
            bundle_digest: publication.record.bundle_digest.clone(),
            spec_digest: publication.record.spec_digest.clone(),
            policy_digest: publication.record.policy_digest.clone(),
            wasm_digest: publication.record.wasm_digest.clone(),
            content_digest: publication.record.content_digest.clone(),
            seed_digest: publication.record.seed_digest.clone(),
            installed_at: publication.record.installed_at.clone().unwrap_or_default(),
            last_reconciled_at: publication.record.last_reconciled_at.clone(),
            status: publication.record.status.clone(),
        });
        let wasm_rows = wasm_modules
            .iter()
            .map(|module| {
                (
                    module.module_name,
                    module.wasm_bytes,
                    module.sha256_hash,
                    module.source,
                )
            })
            .collect::<Vec<_>>();
        PostgresEventStore::publish_specs(
            self,
            tenant,
            &rows,
            mode == SpecPublicationMode::Replace,
            constraints,
            policy,
            os_app.as_ref(),
            &wasm_rows,
            policy_owner,
            &policy_entries,
        )
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
        self.persist_spec_verification(
            tenant,
            entity_type,
            PostgresSpecVerificationUpdate {
                status: update.status,
                verified: update.verified,
                levels_passed: update.levels_passed,
                levels_total: update.levels_total,
                verification_result_json: update.verification_result_json,
            },
        )
        .await
        .map_err(|e| e.to_string())
    }

    async fn upsert_tenant_policy(&self, tenant: &str, policy_text: &str) -> Result<(), String> {
        self.upsert_tenant_policy(tenant, policy_text)
            .await
            .map_err(|e| e.to_string())
    }

    async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), String> {
        self.upsert_tenant_constraints(tenant, cross_invariants_toml)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String> {
        self.load_tenant_policies().await.map_err(|e| e.to_string())
    }

    async fn load_policy_entries(&self) -> Result<Vec<PolicyEntryRow>, String> {
        let rows = self.load_all_policies().await.map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|row| PolicyEntryRow {
                tenant: row.tenant,
                policy_id: row.policy_id,
                cedar_text: row.cedar_text,
                created_by: row.created_by,
                enabled: row.enabled,
            })
            .collect())
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

    async fn record_installed_app_metadata(
        &self,
        record: &InstalledAppRecord,
    ) -> Result<(), String> {
        let row = PostgresInstalledAppRow {
            tenant: record.tenant.clone(),
            app_name: record.app_name.clone(),
            source_kind: record.source_kind.clone(),
            app_ref: record.app_ref.clone(),
            version_hash: record.version_hash.clone(),
            pinned_version_hash: record.pinned_version_hash.clone(),
            current_version_hash: record.current_version_hash.clone(),
            follow_policy: record.follow_policy.clone(),
            closure_id: record.closure_id.clone(),
            registry_url: record.registry_url.clone(),
            registry_tenant: record.registry_tenant.clone(),
            app_version: record.app_version.clone(),
            bundle_digest: record.bundle_digest.clone(),
            spec_digest: record.spec_digest.clone(),
            policy_digest: record.policy_digest.clone(),
            wasm_digest: record.wasm_digest.clone(),
            content_digest: record.content_digest.clone(),
            seed_digest: record.seed_digest.clone(),
            installed_at: record.installed_at.clone().unwrap_or_default(),
            last_reconciled_at: record.last_reconciled_at.clone(),
            status: record.status.clone(),
        };
        self.record_installed_app_metadata(&row)
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_installed_app(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<InstalledAppRecord>, String> {
        self.get_installed_app(tenant, app_name)
            .await
            .map(|row| {
                row.map(|row| InstalledAppRecord {
                    tenant: row.tenant,
                    app_name: row.app_name,
                    source_kind: row.source_kind,
                    app_ref: row.app_ref,
                    version_hash: row.version_hash,
                    pinned_version_hash: row.pinned_version_hash,
                    current_version_hash: row.current_version_hash,
                    follow_policy: row.follow_policy,
                    closure_id: row.closure_id,
                    registry_url: row.registry_url,
                    registry_tenant: row.registry_tenant,
                    app_version: row.app_version,
                    bundle_digest: row.bundle_digest,
                    spec_digest: row.spec_digest,
                    policy_digest: row.policy_digest,
                    wasm_digest: row.wasm_digest,
                    content_digest: row.content_digest,
                    seed_digest: row.seed_digest,
                    installed_at: Some(row.installed_at),
                    last_reconciled_at: row.last_reconciled_at,
                    status: row.status,
                })
            })
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
                source: r.source,
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
                source: r.source,
            })
            .collect())
    }

    async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
        source: &str,
    ) -> Result<(), String> {
        self.upsert_wasm_module(tenant, name, bytes, hash, source)
            .await
            .map_err(|e| e.to_string())
    }
}
