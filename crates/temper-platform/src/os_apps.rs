//! OS App Catalog — agent-installable pre-built application specs.
//!
//! OS apps are spec bundles (IOA TOML + CSDL + Cedar policies) that ship
//! embedded in the binary. Agents discover them via `list_apps()` / `install_app()`
//! and developers can pre-load them with `--os-app <name>`.
//!
//! Install reuses [`crate::bootstrap::bootstrap_tenant_specs`] so every OS app
//! goes through the same verification cascade as system specs.

use serde::Serialize;

use crate::bootstrap;
use crate::state::PlatformState;

// ── Project Management OS App ──────────────────────────────────────

const PM_ISSUE_IOA: &str = include_str!("../../../os-apps/project-management/issue.ioa.toml");
const PM_PROJECT_IOA: &str = include_str!("../../../os-apps/project-management/project.ioa.toml");
const PM_CYCLE_IOA: &str = include_str!("../../../os-apps/project-management/cycle.ioa.toml");
const PM_COMMENT_IOA: &str = include_str!("../../../os-apps/project-management/comment.ioa.toml");
const PM_LABEL_IOA: &str = include_str!("../../../os-apps/project-management/label.ioa.toml");
const PM_CSDL: &str = include_str!("../../../os-apps/project-management/model.csdl.xml");
const PM_CEDAR_ISSUE: &str =
    include_str!("../../../os-apps/project-management/policies/issue.cedar");

// ── Temper FS OS App ───────────────────────────────────────────────

const FS_FILE_IOA: &str = include_str!("../../../os-apps/temper-fs/specs/file.ioa.toml");
const FS_DIR_IOA: &str = include_str!("../../../os-apps/temper-fs/specs/directory.ioa.toml");
const FS_VERSION_IOA: &str = include_str!("../../../os-apps/temper-fs/specs/file_version.ioa.toml");
const FS_WORKSPACE_IOA: &str = include_str!("../../../os-apps/temper-fs/specs/workspace.ioa.toml");
const FS_CSDL: &str = include_str!("../../../os-apps/temper-fs/specs/model.csdl.xml");
const FS_CEDAR_FILE: &str = include_str!("../../../os-apps/temper-fs/policies/file.cedar");
const FS_CEDAR_WORKSPACE: &str =
    include_str!("../../../os-apps/temper-fs/policies/workspace.cedar");
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
            cedar_policies: &[FS_CEDAR_FILE, FS_CEDAR_WORKSPACE],
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
) -> Result<Vec<String>, String> {
    let bundle =
        get_os_app(app_name).ok_or_else(|| format!("OS app '{app_name}' not found in catalog"))?;

    // Build the full Cedar policy text for this tenant (existing + new).
    // Keep this idempotent across repeated installs/restarts by skipping
    // policy blocks that are already present.
    let combined_policy = if !bundle.cedar_policies.is_empty() {
        let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        let existing = policies.get(tenant).cloned().unwrap_or_default();
        let mut full_text = existing.clone();
        for policy in bundle.cedar_policies {
            let candidate = policy.trim();
            if candidate.is_empty() || existing.contains(candidate) || full_text.contains(candidate)
            {
                continue;
            }
            if !full_text.is_empty() {
                full_text.push('\n');
            }
            full_text.push_str(candidate);
        }
        if full_text.trim().is_empty() {
            None
        } else {
            Some(full_text)
        }
    } else {
        None
    };

    // ── Step 1: Persist to Turso FIRST (if available). ──────────────
    // If any write fails, bail before touching in-memory state.
    if let Some(ref store) = state.server.event_store
        && let Some(turso) = store.platform_turso_store()
    {
        for (entity_type, ioa_source) in bundle.specs {
            let hash = temper_store_turso::spec_content_hash(ioa_source);
            turso
                .upsert_spec(tenant, entity_type, ioa_source, bundle.csdl, &hash)
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
    }

    // ── Step 2: Bootstrap into memory (verification + registry). ────
    let verified_cache = if let Some(ref store) = state.server.event_store
        && let Some(turso) = store.platform_turso_store()
    {
        turso
            .load_verification_cache(tenant)
            .await
            .unwrap_or_default()
    } else {
        std::collections::BTreeMap::new()
    };
    bootstrap::bootstrap_tenant_specs(
        state,
        tenant,
        bundle.csdl,
        bundle.specs,
        &format!("OS-App({app_name})"),
        &verified_cache,
    );

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

    let entity_types = bundle
        .specs
        .iter()
        .map(|(name, _)| name.to_string())
        .collect();

    tracing::info!(
        "Installed OS app '{app_name}' for tenant '{tenant}': {:?}",
        bundle.specs.iter().map(|(t, _)| *t).collect::<Vec<_>>()
    );

    Ok(entity_types)
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::tenant::TenantId;
    use temper_spec::automaton;
    use temper_spec::csdl::parse_csdl;
    use temper_verify::cascade::VerificationCascade;

    #[test]
    fn test_pm_specs_parse() {
        for (entity_type, ioa_source) in PM_SPECS {
            let result = automaton::parse_automaton(ioa_source);
            assert!(
                result.is_ok(),
                "PM spec {} failed to parse: {:?}",
                entity_type,
                result.err()
            );
        }
    }

    #[test]
    fn test_pm_csdl_parses() {
        let result = parse_csdl(PM_CSDL);
        assert!(
            result.is_ok(),
            "PM CSDL failed to parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_pm_spec_entity_names() {
        for (entity_type, ioa_source) in PM_SPECS {
            let a = automaton::parse_automaton(ioa_source).unwrap();
            assert_eq!(
                a.automaton.name, *entity_type,
                "PM spec name mismatch: expected {entity_type}, got {}",
                a.automaton.name
            );
        }
    }

    #[test]
    fn test_pm_specs_verify() {
        for (entity_type, ioa_source) in PM_SPECS {
            let cascade = VerificationCascade::from_ioa(ioa_source)
                .with_sim_seeds(3)
                .with_prop_test_cases(50);
            let result = cascade.run();
            assert!(
                result.all_passed,
                "PM spec {} failed verification",
                entity_type
            );
        }
    }

    #[test]
    fn test_list_os_apps_returns_catalog() {
        let apps = list_os_apps();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "project-management");
        assert_eq!(apps[0].entity_types.len(), 5);
        assert_eq!(apps[1].name, "temper-fs");
        assert_eq!(apps[1].entity_types.len(), 4);
    }

    #[test]
    fn test_get_os_app_project_management() {
        let bundle = get_os_app("project-management");
        assert!(bundle.is_some());
        let bundle = bundle.unwrap();
        assert_eq!(bundle.specs.len(), 5);
        assert!(!bundle.csdl.is_empty());
        assert_eq!(bundle.cedar_policies.len(), 1);
    }

    #[test]
    fn test_get_os_app_nonexistent() {
        assert!(get_os_app("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_install_os_app_registers_entities() {
        let state = PlatformState::new(None);
        let result = install_os_app(&state, "test-pm", "project-management").await;
        assert!(result.is_ok());
        let entities = result.unwrap();
        assert_eq!(entities.len(), 5);
        assert!(entities.contains(&"Issue".to_string()));
        assert!(entities.contains(&"Project".to_string()));
        assert!(entities.contains(&"Cycle".to_string()));
        assert!(entities.contains(&"Comment".to_string()));
        assert!(entities.contains(&"Label".to_string()));

        // Verify entities are in the registry.
        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("test-pm");
        assert!(registry.get_table(&tenant, "Issue").is_some());
        assert!(registry.get_table(&tenant, "Project").is_some());
        assert!(registry.get_table(&tenant, "Cycle").is_some());
        assert!(registry.get_table(&tenant, "Comment").is_some());
        assert!(registry.get_table(&tenant, "Label").is_some());
    }

    #[tokio::test]
    async fn test_install_os_app_nonexistent_returns_error() {
        let state = PlatformState::new(None);
        let result = install_os_app(&state, "test", "nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found in catalog"));
    }

    #[tokio::test]
    async fn test_install_os_app_policy_merge_is_idempotent() {
        let state = PlatformState::new(None);
        install_os_app(&state, "test-ws", "project-management")
            .await
            .expect("first install");
        let first_policy = {
            let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
            policies.get("test-ws").expect("tenant policy").clone()
        };

        install_os_app(&state, "test-ws", "project-management")
            .await
            .expect("second install");

        let second_policy = {
            let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
            policies.get("test-ws").expect("tenant policy").clone()
        };
        assert_eq!(first_policy, second_policy);
    }

    /// Proves the full install → persist → reboot → restore cycle.
    ///
    /// 1. Install OS app with a real Turso-backed SQLite DB.
    /// 2. Verify specs land in both registry and Turso.
    /// 3. Build a fresh PlatformState (simulating restart) with the same DB.
    /// 4. Restore registry from Turso.
    /// 5. Verify specs survived the "restart".
    #[tokio::test]
    async fn test_os_app_install_survives_restart() {
        use std::sync::Arc;
        use temper_server::event_store::ServerEventStore;
        use temper_server::registry_bootstrap::restore_registry_from_turso;
        use temper_store_turso::TursoEventStore;

        // Use a unique temp file DB for this test.
        let db_path = format!("/tmp/temper-test-{}.db", uuid::Uuid::new_v4());
        let db_url = format!("file:{db_path}");

        // ── Phase A: Install into a fresh state with Turso. ─────────
        let turso = TursoEventStore::new(&db_url, None).await.unwrap();
        let mut state = PlatformState::new(None);
        state.server.event_store = Some(Arc::new(ServerEventStore::Turso(turso)));

        let result = install_os_app(&state, "test-ws", "project-management").await;
        assert!(result.is_ok(), "install failed: {:?}", result.err());
        let entities = result.unwrap();
        assert_eq!(entities.len(), 5);

        // Verify specs are in the in-memory registry.
        {
            let registry = state.registry.read().unwrap();
            let tenant = TenantId::new("test-ws");
            assert!(registry.get_table(&tenant, "Issue").is_some());
            assert!(registry.get_table(&tenant, "Project").is_some());
        }

        // Verify specs are persisted to Turso.
        let turso_ref = state
            .server
            .event_store
            .as_ref()
            .unwrap()
            .platform_turso_store()
            .unwrap();
        let rows = turso_ref.load_specs().await.unwrap();
        assert!(
            rows.iter()
                .any(|r| r.tenant == "test-ws" && r.entity_type == "Issue"),
            "Issue spec not found in Turso"
        );

        // Verify installed_apps record is in Turso.
        let installed = turso_ref.list_all_installed_apps().await.unwrap();
        assert!(
            installed.contains(&("test-ws".to_string(), "project-management".to_string())),
            "installed app record not found"
        );

        // ── Phase B: Simulate restart — fresh state, same DB. ───────
        let turso2 = TursoEventStore::new(&db_url, None).await.unwrap();
        let state2 = PlatformState::new(None);
        // Verify fresh registry is empty for this tenant.
        {
            let registry = state2.registry.read().unwrap();
            let tenant = TenantId::new("test-ws");
            assert!(
                registry.get_table(&tenant, "Issue").is_none(),
                "fresh registry should be empty"
            );
        }

        // Restore from Turso (this is what build_registry does on boot).
        // Fetch async data outside the lock, then assign synchronously to avoid
        // holding a RwLockWriteGuard across an await point.
        {
            use temper_server::registry::SpecRegistry;
            let mut temp_registry = SpecRegistry::new();
            let restored = restore_registry_from_turso(&mut temp_registry, &turso2)
                .await
                .unwrap();
            assert!(restored > 0, "expected restored specs, got 0");
            *state2.registry.write().unwrap() = temp_registry;
        }

        // Verify specs survived the restart.
        {
            let registry = state2.registry.read().unwrap();
            let tenant = TenantId::new("test-ws");
            assert!(registry.get_table(&tenant, "Issue").is_some());
            assert!(registry.get_table(&tenant, "Project").is_some());
            assert!(registry.get_table(&tenant, "Cycle").is_some());
            assert!(registry.get_table(&tenant, "Comment").is_some());
            assert!(registry.get_table(&tenant, "Label").is_some());
        }

        // Clean up temp DB.
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path}-wal"));
        let _ = std::fs::remove_file(format!("{db_path}-shm"));
    }
}
