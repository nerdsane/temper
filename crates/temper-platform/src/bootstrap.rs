//! System tenant bootstrap.
//!
//! Loads the platform's own entity specs (Project, Tenant, CatalogEntry,
//! Collaborator, Version), runs the verification cascade, and registers
//! them as the `temper-system` tenant. This is dogfooding: the platform
//! manages itself using its own framework.

use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_spec::automaton;
use temper_spec::csdl::parse_csdl;
use temper_verify::cascade::VerificationCascade;

use crate::state::PlatformState;

/// System tenant ID.
pub const SYSTEM_TENANT: &str = "temper-system";

// Embed system specs at compile time.
const PROJECT_IOA: &str = include_str!("specs/Project.ioa.toml");
const TENANT_IOA: &str = include_str!("specs/Tenant.ioa.toml");
const CATALOG_ENTRY_IOA: &str = include_str!("specs/CatalogEntry.ioa.toml");
const COLLABORATOR_IOA: &str = include_str!("specs/Collaborator.ioa.toml");
const VERSION_IOA: &str = include_str!("specs/Version.ioa.toml");
const SYSTEM_CSDL: &str = include_str!("specs/model.csdl.xml");

/// All system entity specs as (entity_type, ioa_source) pairs.
const SYSTEM_SPECS: &[(&str, &str)] = &[
    ("Project", PROJECT_IOA),
    ("Tenant", TENANT_IOA),
    ("CatalogEntry", CATALOG_ENTRY_IOA),
    ("Collaborator", COLLABORATOR_IOA),
    ("Version", VERSION_IOA),
];

/// Bootstrap the system tenant.
///
/// 1. Validates all system specs parse correctly
/// 2. Runs verification cascade on each (should always pass — specs are curated)
/// 3. Registers `temper-system` tenant in the SpecRegistry
///
/// Panics if system specs fail to parse or verify (this is a fatal startup error).
pub fn bootstrap_system_tenant(state: &PlatformState) {
    tracing::info!(
        "Bootstrapping temper-system tenant with {} entities",
        SYSTEM_SPECS.len()
    );

    // Validate all specs parse
    for (entity_type, ioa_source) in SYSTEM_SPECS {
        automaton::parse_automaton(ioa_source)
            .unwrap_or_else(|e| panic!("System spec {entity_type} failed to parse: {e}"));
    }

    // Run verification cascade on each (lightweight — system specs are simple)
    for (entity_type, ioa_source) in SYSTEM_SPECS {
        let cascade = VerificationCascade::from_ioa(ioa_source)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);
        let result = cascade.run();
        assert!(
            result.all_passed,
            "System spec {entity_type} failed verification cascade"
        );
    }

    // Parse system CSDL
    let csdl = parse_csdl(SYSTEM_CSDL).expect("System CSDL failed to parse");

    // Register system tenant
    let system_tid = TenantId::new(SYSTEM_TENANT);
    {
        let mut registry = state.registry.write().unwrap();
        registry.register_tenant(
            system_tid.clone(),
            csdl,
            SYSTEM_CSDL.to_string(),
            SYSTEM_SPECS,
        );
        // Mark all system entities as pre-verified (they passed the cascade above).
        let now = temper_runtime::scheduler::sim_now().to_rfc3339();
        for (entity_type, _) in SYSTEM_SPECS {
            registry.set_verification_status(
                &system_tid,
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "Bootstrap".to_string(),
                        passed: true,
                        summary: "Pre-verified at bootstrap".to_string(),
                        details: None,
                    }],
                    verified_at: now.clone(),
                }),
            );
        }
    }

    tracing::info!(
        "temper-system tenant bootstrapped: {:?}",
        SYSTEM_SPECS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_specs_parse() {
        for (entity_type, ioa_source) in SYSTEM_SPECS {
            let result = automaton::parse_automaton(ioa_source);
            assert!(
                result.is_ok(),
                "System spec {} failed to parse: {:?}",
                entity_type,
                result.err()
            );
        }
    }

    #[test]
    fn test_system_csdl_parses() {
        let result = parse_csdl(SYSTEM_CSDL);
        assert!(
            result.is_ok(),
            "System CSDL failed to parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_bootstrap_registers_system_tenant() {
        let state = PlatformState::new(None);

        bootstrap_system_tenant(&state);

        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new(SYSTEM_TENANT);

        assert!(registry.get_tenant(&tenant).is_some());
        assert!(registry.get_table(&tenant, "Project").is_some());
        assert!(registry.get_table(&tenant, "Tenant").is_some());
        assert!(registry.get_table(&tenant, "CatalogEntry").is_some());
        assert!(registry.get_table(&tenant, "Collaborator").is_some());
        assert!(registry.get_table(&tenant, "Version").is_some());
    }

    #[test]
    fn test_system_spec_entity_names() {
        for (entity_type, ioa_source) in SYSTEM_SPECS {
            let automaton = automaton::parse_automaton(ioa_source).unwrap();
            assert_eq!(
                automaton.automaton.name, *entity_type,
                "Spec name mismatch: expected {entity_type}, got {}",
                automaton.automaton.name
            );
        }
    }

    #[test]
    fn test_system_specs_verify() {
        for (entity_type, ioa_source) in SYSTEM_SPECS {
            let cascade = VerificationCascade::from_ioa(ioa_source)
                .with_sim_seeds(3)
                .with_prop_test_cases(50);
            let result = cascade.run();
            assert!(
                result.all_passed,
                "System spec {} failed verification",
                entity_type
            );
        }
    }

    #[test]
    fn test_project_initial_state() {
        let automaton = automaton::parse_automaton(PROJECT_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Created");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_tenant_initial_state() {
        let automaton = automaton::parse_automaton(TENANT_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Pending");
        assert_eq!(automaton.automaton.states.len(), 5);
    }

    #[test]
    fn test_entity_types_count() {
        assert_eq!(SYSTEM_SPECS.len(), 5);
    }
}
