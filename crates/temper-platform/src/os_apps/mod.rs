//! OS App Catalog — agent-installable pre-built application specs.
//!
//! OS apps are spec bundles (IOA TOML + CSDL + Cedar policies) that ship
//! embedded in the binary. Agents discover them via `list_apps()` / `install_app()`
//! and developers can pre-load them with `--os-app <name>`.
//!
//! Install reuses [`crate::bootstrap::bootstrap_tenant_specs`] so every OS app
//! goes through the same verification cascade as system specs.

use std::collections::BTreeMap;

use serde::Serialize;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{emit_csdl_xml, merge_csdl, parse_csdl};

use crate::bootstrap;
use crate::state::PlatformState;

/// Result of an OS app installation, categorising each spec by what happened.
#[derive(Debug, Clone, Serialize)]
pub struct InstallResult {
    /// Entity types registered for the first time.
    pub added: Vec<String>,
    /// Entity types that already existed but whose IOA source changed.
    pub updated: Vec<String>,
    /// Entity types whose IOA source was byte-for-byte identical — skipped.
    pub skipped: Vec<String>,
}

// ── Project Management OS App ──────────────────────────────────────

const PM_ISSUE_IOA: &str = include_str!("../../../../os-apps/project-management/issue.ioa.toml");
const PM_PROJECT_IOA: &str =
    include_str!("../../../../os-apps/project-management/project.ioa.toml");
const PM_CYCLE_IOA: &str = include_str!("../../../../os-apps/project-management/cycle.ioa.toml");
const PM_COMMENT_IOA: &str =
    include_str!("../../../../os-apps/project-management/comment.ioa.toml");
const PM_LABEL_IOA: &str = include_str!("../../../../os-apps/project-management/label.ioa.toml");
const PM_CSDL: &str = include_str!("../../../../os-apps/project-management/model.csdl.xml");
const PM_CEDAR_ISSUE: &str =
    include_str!("../../../../os-apps/project-management/policies/issue.cedar");

// ── Temper FS OS App ───────────────────────────────────────────────

const FS_FILE_IOA: &str = include_str!("../../../../os-apps/temper-fs/specs/file.ioa.toml");
const FS_DIR_IOA: &str = include_str!("../../../../os-apps/temper-fs/specs/directory.ioa.toml");
const FS_VERSION_IOA: &str =
    include_str!("../../../../os-apps/temper-fs/specs/file_version.ioa.toml");
const FS_WORKSPACE_IOA: &str =
    include_str!("../../../../os-apps/temper-fs/specs/workspace.ioa.toml");
const FS_CSDL: &str = include_str!("../../../../os-apps/temper-fs/specs/model.csdl.xml");
const FS_CEDAR_FILE: &str = include_str!("../../../../os-apps/temper-fs/policies/file.cedar");
const FS_CEDAR_WORKSPACE: &str =
    include_str!("../../../../os-apps/temper-fs/policies/workspace.cedar");
const FS_CEDAR_WASM: &str = include_str!("../../../../os-apps/temper-fs/policies/wasm.cedar");

// ── Agent Orchestration OS App ────────────────────────────────────

const AO_HEARTBEAT_IOA: &str =
    include_str!("../../../../os-apps/agent-orchestration/specs/heartbeat_run.ioa.toml");
const AO_ORG_IOA: &str =
    include_str!("../../../../os-apps/agent-orchestration/specs/organization.ioa.toml");
const AO_BUDGET_IOA: &str =
    include_str!("../../../../os-apps/agent-orchestration/specs/budget_ledger.ioa.toml");
const AO_CSDL: &str = include_str!("../../../../os-apps/agent-orchestration/specs/model.csdl.xml");
const AO_CEDAR: &str =
    include_str!("../../../../os-apps/agent-orchestration/policies/orchestration.cedar");

// ── Temper Agent OS App ──────────────────────────────────────────────

const TEMPER_AGENT_IOA: &str =
    include_str!("../../../../os-apps/temper-agent/specs/temper_agent.ioa.toml");
const TEMPER_AGENT_CSDL: &str =
    include_str!("../../../../os-apps/temper-agent/specs/model.csdl.xml");
const TEMPER_AGENT_CEDAR: &str =
    include_str!("../../../../os-apps/temper-agent/policies/agent.cedar");

/// Metadata for an OS app in the catalog.
#[derive(Debug, Clone, Serialize)]
pub struct OsAppEntry {
    /// Short name used in CLI flags and API calls (e.g. `"project-management"`).
    pub name: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Entity types included in the app.
    pub entity_types: &'static [&'static str],
    /// Semantic version.
    pub version: &'static str,
}

/// Full spec bundle for an OS app.
pub struct OsAppBundle {
    /// IOA spec sources as `(entity_type, ioa_toml_source)` pairs.
    pub specs: &'static [(&'static str, &'static str)],
    /// CSDL XML source.
    pub csdl: &'static str,
    /// Cedar policy sources (may be empty).
    pub cedar_policies: &'static [&'static str],
}

/// Project Management app specs.
const PM_SPECS: &[(&str, &str)] = &[
    ("Issue", PM_ISSUE_IOA),
    ("Project", PM_PROJECT_IOA),
    ("Cycle", PM_CYCLE_IOA),
    ("Comment", PM_COMMENT_IOA),
    ("Label", PM_LABEL_IOA),
];

/// Temper FS app specs.
const FS_SPECS: &[(&str, &str)] = &[
    ("File", FS_FILE_IOA),
    ("Directory", FS_DIR_IOA),
    ("FileVersion", FS_VERSION_IOA),
    ("Workspace", FS_WORKSPACE_IOA),
];

/// Agent orchestration app specs.
const AO_SPECS: &[(&str, &str)] = &[
    ("HeartbeatRun", AO_HEARTBEAT_IOA),
    ("Organization", AO_ORG_IOA),
    ("BudgetLedger", AO_BUDGET_IOA),
];

/// Temper Agent app specs.
const TEMPER_AGENT_SPECS: &[(&str, &str)] = &[("TemperAgent", TEMPER_AGENT_IOA)];

/// All available OS apps.
static OS_APP_CATALOG: &[OsAppEntry] = &[
    OsAppEntry {
        name: "project-management",
        description: "Issue tracking with projects, cycles, labels, and comments",
        entity_types: &["Issue", "Project", "Cycle", "Comment", "Label"],
        version: "0.1.0",
    },
    OsAppEntry {
        name: "temper-fs",
        description: "Governed filesystem with workspaces, directories, files, and versioning",
        entity_types: &["File", "Directory", "FileVersion", "Workspace"],
        version: "0.1.0",
    },
    OsAppEntry {
        name: "agent-orchestration",
        description: "Agent heartbeat orchestration with organizations and budget ledgering",
        entity_types: &["HeartbeatRun", "Organization", "BudgetLedger"],
        version: "0.1.0",
    },
    OsAppEntry {
        name: "temper-agent",
        description: "Spec-driven agent with LLM loop, sandbox tools, and TemperFS conversation storage",
        entity_types: &["TemperAgent"],
        version: "0.1.0",
    },
];

/// List all available OS apps.
pub fn list_os_apps() -> &'static [OsAppEntry] {
    OS_APP_CATALOG
}

/// Get the full spec bundle for an OS app by name.
pub fn get_os_app(name: &str) -> Option<OsAppBundle> {
    match name {
        "project-management" => Some(OsAppBundle {
            specs: PM_SPECS,
            csdl: PM_CSDL,
            cedar_policies: &[PM_CEDAR_ISSUE],
        }),
        "temper-fs" => Some(OsAppBundle {
            specs: FS_SPECS,
            csdl: FS_CSDL,
            cedar_policies: &[FS_CEDAR_FILE, FS_CEDAR_WORKSPACE, FS_CEDAR_WASM],
        }),
        "agent-orchestration" => Some(OsAppBundle {
            specs: AO_SPECS,
            csdl: AO_CSDL,
            cedar_policies: &[AO_CEDAR],
        }),
        "temper-agent" => Some(OsAppBundle {
            specs: TEMPER_AGENT_SPECS,
            csdl: TEMPER_AGENT_CSDL,
            cedar_policies: &[TEMPER_AGENT_CEDAR],
        }),
        _ => None,
    }
}

/// Install an OS app into a tenant (workspace).
///
/// Runs the verification cascade and registers specs in the SpecRegistry,
/// loads Cedar policies, and **persists everything to the platform DB** so
/// specs survive redeployments.
///
/// **Write ordering:** Turso first, then memory. If Turso persistence fails
/// the operation returns an error *before* touching in-memory state, so the
/// registry and Cedar engine stay consistent with the durable store.
pub async fn install_os_app(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<InstallResult, String> {
    let bundle =
        get_os_app(app_name).ok_or_else(|| format!("OS app '{app_name}' not found in catalog"))?;
    let tenant_id = TenantId::new(tenant);

    // Classify each bundle spec as added / updated / skipped, and compute the
    // merged CSDL — both require the registry read lock, so we do them together.
    let (mut added, mut updated, mut skipped, merged_csdl) = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        let mut added = Vec::new();
        let mut updated = Vec::new();
        let mut skipped = Vec::new();
        for (entity_type, ioa_source) in bundle.specs {
            let incoming_hash = temper_store_turso::spec_content_hash(ioa_source);
            match registry.get_spec(&tenant_id, entity_type) {
                Some(existing) => {
                    let existing_hash = temper_store_turso::spec_content_hash(&existing.ioa_source);
                    if incoming_hash == existing_hash {
                        skipped.push(entity_type.to_string());
                    } else {
                        updated.push(entity_type.to_string());
                    }
                }
                None => {
                    added.push(entity_type.to_string());
                }
            }
        }
        // OS app installs must preserve existing tenant types.
        let merged_csdl = if let Some(existing) = registry.get_tenant(&tenant_id) {
            let incoming = parse_csdl(bundle.csdl)
                .map_err(|e| format!("Failed to parse CSDL for OS app '{app_name}': {e}"))?;
            emit_csdl_xml(&merge_csdl(&existing.csdl, &incoming))
        } else {
            bundle.csdl.to_string()
        };
        (added, updated, skipped, merged_csdl)
    };
    // Sort for deterministic output.
    added.sort();
    updated.sort();
    skipped.sort();

    // Build the full Cedar policy text for this tenant (existing + new).
    let combined_policy = if !bundle.cedar_policies.is_empty() {
        let combined: String = bundle.cedar_policies.join("\n");
        let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        let existing = policies.get(tenant).cloned().unwrap_or_default();
        let full_text = if existing.is_empty() {
            combined
        } else {
            format!("{existing}\n{combined}")
        };
        Some(full_text)
    } else {
        None
    };

    // ── Step 1: Persist to Turso FIRST (if available). ──────────────
    // If any write fails, bail before touching in-memory state.
    if let Some(ref store) = state.server.event_store
        && let Some(turso) = store.platform_turso_store()
    {
        let mut spec_sources: BTreeMap<String, String> = turso
            .load_specs()
            .await
            .map_err(|e| format!("Failed to load existing specs for tenant '{tenant}': {e}"))?
            .into_iter()
            .filter(|row| row.tenant == tenant)
            .map(|row| (row.entity_type, row.ioa_source))
            .collect();

        for (entity_type, ioa_source) in bundle.specs {
            spec_sources.insert((*entity_type).to_string(), (*ioa_source).to_string());
        }

        for (entity_type, ioa_source) in spec_sources {
            let hash = temper_store_turso::spec_content_hash(&ioa_source);
            turso
                .upsert_spec(tenant, &entity_type, &ioa_source, &merged_csdl, &hash)
                .await
                .map_err(|e| format!("Failed to persist spec {entity_type}: {e}"))?;
        }
        if let Some(ref policy_text) = combined_policy {
            turso
                .upsert_tenant_policy(tenant, policy_text)
                .await
                .map_err(|e| format!("Failed to persist Cedar policy: {e}"))?;
        }
        turso
            .record_installed_app(tenant, app_name)
            .await
            .map_err(|e| format!("Failed to record app installation: {e}"))?;
        // Commit all specs atomically after all writes succeed.
        turso
            .commit_specs(tenant)
            .await
            .map_err(|e| format!("Failed to commit specs: {e}"))?;
    } else if let Some(ref store) = state.server.event_store
        && let Some(ps) = store.platform_store()
    {
        for (entity_type, ioa_source) in bundle.specs {
            let hash = temper_store_turso::spec_content_hash(ioa_source);
            ps.upsert_spec(tenant, entity_type, ioa_source, &merged_csdl, &hash)
                .await
                .map_err(|e| format!("Failed to persist spec {entity_type}: {e}"))?;
        }
        if let Some(ref policy_text) = combined_policy {
            ps.upsert_tenant_policy(tenant, policy_text)
                .await
                .map_err(|e| format!("Failed to persist Cedar policy: {e}"))?;
        }
        ps.record_installed_app(tenant, app_name)
            .await
            .map_err(|e| format!("Failed to record app installation: {e}"))?;
        // Commit all specs atomically after all writes succeed.
        ps.commit_specs(tenant)
            .await
            .map_err(|e| format!("Failed to commit specs: {e}"))?;
    }

    // ── Step 2: Bootstrap into memory (verification + registry). ────
    // Only process specs whose content has changed (added or updated);
    // skipped specs are already loaded with identical content.
    let specs_to_bootstrap: Vec<(&str, &str)> = bundle
        .specs
        .iter()
        .filter(|(entity_type, _)| !skipped.contains(&entity_type.to_string()))
        .map(|(et, src)| (*et, *src))
        .collect();

    if !specs_to_bootstrap.is_empty() {
        let verified_cache = if let Some(ref store) = state.server.event_store
            && let Some(turso) = store.platform_turso_store()
        {
            turso
                .load_verification_cache(tenant)
                .await
                .unwrap_or_default()
        } else if let Some(ref store) = state.server.event_store
            && let Some(ps) = store.platform_store()
        {
            ps.load_verification_cache(tenant).await.unwrap_or_default()
        } else {
            std::collections::BTreeMap::new()
        };
        bootstrap::bootstrap_tenant_specs(
            state,
            tenant,
            &merged_csdl,
            &specs_to_bootstrap,
            true,
            &format!("OS-App({app_name})"),
            &verified_cache,
        );
    }

    // ── Step 3: Load Cedar policies into memory. ────────────────────
    if let Some(ref policy_text) = combined_policy {
        let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.to_string(), policy_text.clone());
        // Rebuild the authorization engine with all policies.
        let mut all_policies = String::new();
        for text in policies.values() {
            all_policies.push_str(text);
            all_policies.push('\n');
        }
        if let Err(e) = state.server.authz.reload_policies(&all_policies) {
            tracing::warn!("Failed to reload Cedar policies after OS app install: {e}");
        }
    }

    tracing::info!(
        "Installed OS app '{app_name}' for tenant '{tenant}': \
         added={:?} updated={:?} skipped={:?}",
        added,
        updated,
        skipped,
    );

    Ok(InstallResult {
        added,
        updated,
        skipped,
    })
}

#[cfg(test)]
mod tests;
