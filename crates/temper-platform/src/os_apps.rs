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

/// All available OS apps.
static OS_APP_CATALOG: &[OsAppEntry] = &[OsAppEntry {
    name: "project-management",
    description: "Issue tracking with projects, cycles, labels, and comments",
    entity_types: &["Issue", "Project", "Cycle", "Comment", "Label"],
    version: "0.1.0",
}];

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
        _ => None,
    }
}

/// Install an OS app into a tenant.
///
/// Runs the verification cascade and registers specs in the SpecRegistry,
/// then loads Cedar policies. Returns the list of installed entity types.
pub fn install_os_app(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<Vec<String>, String> {
    let bundle =
        get_os_app(app_name).ok_or_else(|| format!("OS app '{app_name}' not found in catalog"))?;

    // Reuse the same bootstrap path as system/agent specs.
    bootstrap::bootstrap_tenant_specs(
        state,
        tenant,
        bundle.csdl,
        bundle.specs,
        &format!("OS-App({app_name})"),
    );

    // Load Cedar policies for the tenant.
    if !bundle.cedar_policies.is_empty() {
        let combined: String = bundle.cedar_policies.join("\n");
        let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        let existing = policies.entry(tenant.to_string()).or_default();
        existing.push_str(&combined);
        existing.push('\n');
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
    fn test_list_os_apps_returns_project_management() {
        let apps = list_os_apps();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "project-management");
        assert_eq!(apps[0].entity_types.len(), 5);
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

    #[test]
    fn test_install_os_app_registers_entities() {
        let state = PlatformState::new(None);
        let result = install_os_app(&state, "test-pm", "project-management");
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

    #[test]
    fn test_install_os_app_nonexistent_returns_error() {
        let state = PlatformState::new(None);
        let result = install_os_app(&state, "test", "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found in catalog"));
    }
}
