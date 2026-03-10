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
const OBSERVATION_IOA: &str = include_str!("specs/Observation.ioa.toml");
const PROBLEM_IOA: &str = include_str!("specs/Problem.ioa.toml");
const ANALYSIS_IOA: &str = include_str!("specs/Analysis.ioa.toml");
const EVOLUTION_DECISION_IOA: &str = include_str!("specs/EvolutionDecision.ioa.toml");
const INSIGHT_IOA: &str = include_str!("specs/Insight.ioa.toml");
const FEATURE_REQUEST_IOA: &str = include_str!("specs/FeatureRequest.ioa.toml");
const GOVERNANCE_DECISION_IOA: &str = include_str!("specs/GovernanceDecision.ioa.toml");
const SYSTEM_CSDL: &str = include_str!("specs/model.csdl.xml");

/// All system entity specs as (entity_type, ioa_source) pairs.
const SYSTEM_SPECS: &[(&str, &str)] = &[
    ("Project", PROJECT_IOA),
    ("Tenant", TENANT_IOA),
    ("CatalogEntry", CATALOG_ENTRY_IOA),
    ("Collaborator", COLLABORATOR_IOA),
    ("Version", VERSION_IOA),
    ("Observation", OBSERVATION_IOA),
    ("Problem", PROBLEM_IOA),
    ("Analysis", ANALYSIS_IOA),
    ("EvolutionDecision", EVOLUTION_DECISION_IOA),
    ("Insight", INSIGHT_IOA),
    ("FeatureRequest", FEATURE_REQUEST_IOA),
    ("GovernanceDecision", GOVERNANCE_DECISION_IOA),
];

// Embed agent specs at compile time.
const AGENT_IOA: &str = include_str!("specs/agent.ioa.toml");
const AGENT_TYPE_IOA: &str = include_str!("specs/agent_type.ioa.toml");
const PLAN_IOA: &str = include_str!("specs/plan.ioa.toml");
const TASK_IOA: &str = include_str!("specs/task.ioa.toml");
const TOOL_CALL_IOA: &str = include_str!("specs/tool_call.ioa.toml");
const SCHEDULE_IOA: &str = include_str!("specs/schedule.ioa.toml");
const HARNESS_IOA: &str = include_str!("specs/harness.ioa.toml");
const POLICY_IOA: &str = include_str!("specs/policy.ioa.toml");
const AGENT_CSDL: &str = include_str!("specs/agent_model.csdl.xml");

/// Agent entity specs as (entity_type, ioa_source) pairs.
const AGENT_SPECS: &[(&str, &str)] = &[
    ("Agent", AGENT_IOA),
    ("AgentType", AGENT_TYPE_IOA),
    ("Plan", PLAN_IOA),
    ("Task", TASK_IOA),
    ("ToolCall", TOOL_CALL_IOA),
    ("Schedule", SCHEDULE_IOA),
    ("Harness", HARNESS_IOA),
    ("Policy", POLICY_IOA),
];

/// Verify, parse, and register a set of IOA specs under a tenant.
///
/// Shared routine that eliminates duplication between system and agent bootstrap:
/// 1. Validates all specs parse correctly
/// 2. Runs verification cascade on each
/// 3. Parses the CSDL schema
/// 4. Registers the tenant in the SpecRegistry
/// 5. Marks all entities as pre-verified
///
/// Panics if any spec fails to parse or verify (fatal startup error).
pub(crate) fn bootstrap_tenant_specs(
    state: &PlatformState,
    tenant: &str,
    csdl_source: &str,
    specs: &[(&str, &str)],
    label: &str,
) {
    tracing::info!(
        "Bootstrapping {label} specs for tenant '{tenant}' with {} entities",
        specs.len()
    );

    // Validate all specs parse.
    for (entity_type, ioa_source) in specs {
        automaton::parse_automaton(ioa_source)
            .unwrap_or_else(|e| panic!("{label} spec {entity_type} failed to parse: {e}"));
    }

    // Run verification cascade on each.
    for (entity_type, ioa_source) in specs {
        let cascade = VerificationCascade::from_ioa(ioa_source)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);
        let result = cascade.run();
        assert!(
            result.all_passed,
            "{label} spec {entity_type} failed verification cascade"
        );
    }

    // Parse CSDL schema.
    let csdl =
        parse_csdl(csdl_source).unwrap_or_else(|e| panic!("{label} CSDL failed to parse: {e}"));

    // Register tenant and mark specs as pre-verified.
    let tenant_id = TenantId::new(tenant);
    {
        let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
        registry.register_tenant(tenant_id.clone(), csdl, csdl_source.to_string(), specs);
        let now = temper_runtime::scheduler::sim_now().to_rfc3339();
        for (entity_type, _) in specs {
            registry.set_verification_status(
                &tenant_id,
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
        "{label} specs bootstrapped for tenant '{tenant}': {:?}",
        specs.iter().map(|(t, _)| *t).collect::<Vec<_>>()
    );
}

/// Bootstrap the system tenant.
///
/// Validates, verifies, and registers all temper-system entity specs.
/// Panics if system specs fail to parse or verify (fatal startup error).
pub fn bootstrap_system_tenant(state: &PlatformState) {
    bootstrap_tenant_specs(state, SYSTEM_TENANT, SYSTEM_CSDL, SYSTEM_SPECS, "System");
}

/// Bootstrap agent entity specs (Agent, Plan, Task, ToolCall) for a tenant.
///
/// Parses and verifies the agent IOA specs, then registers them under the
/// given tenant. Panics if agent specs fail to parse or verify.
pub fn bootstrap_agent_specs(state: &PlatformState, tenant: &str) {
    bootstrap_tenant_specs(state, tenant, AGENT_CSDL, AGENT_SPECS, "Agent");
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
        assert_eq!(SYSTEM_SPECS.len(), 12);
    }

    #[test]
    fn test_observation_initial_state() {
        let automaton = automaton::parse_automaton(OBSERVATION_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_problem_initial_state() {
        let automaton = automaton::parse_automaton(PROBLEM_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_analysis_initial_state() {
        let automaton = automaton::parse_automaton(ANALYSIS_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_evolution_decision_initial_state() {
        let automaton = automaton::parse_automaton(EVOLUTION_DECISION_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_insight_initial_state() {
        let automaton = automaton::parse_automaton(INSIGHT_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_feature_request_initial_state() {
        let automaton = automaton::parse_automaton(FEATURE_REQUEST_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 5);
    }

    #[test]
    fn test_governance_decision_initial_state() {
        let automaton = automaton::parse_automaton(GOVERNANCE_DECISION_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Pending");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_bootstrap_registers_new_entities() {
        let state = PlatformState::new(None);

        bootstrap_system_tenant(&state);

        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new(SYSTEM_TENANT);

        assert!(registry.get_table(&tenant, "Observation").is_some());
        assert!(registry.get_table(&tenant, "Problem").is_some());
        assert!(registry.get_table(&tenant, "Analysis").is_some());
        assert!(registry.get_table(&tenant, "EvolutionDecision").is_some());
        assert!(registry.get_table(&tenant, "Insight").is_some());
        assert!(registry.get_table(&tenant, "FeatureRequest").is_some());
        assert!(registry.get_table(&tenant, "GovernanceDecision").is_some());
    }

    // ── Agent Spec Tests ────────────────────────────────────────────

    #[test]
    fn test_agent_specs_parse() {
        for (entity_type, ioa_source) in AGENT_SPECS {
            let result = automaton::parse_automaton(ioa_source);
            assert!(
                result.is_ok(),
                "Agent spec {} failed to parse: {:?}",
                entity_type,
                result.err()
            );
        }
    }

    #[test]
    fn test_agent_csdl_parses() {
        let result = parse_csdl(AGENT_CSDL);
        assert!(
            result.is_ok(),
            "Agent CSDL failed to parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_agent_spec_entity_names() {
        for (entity_type, ioa_source) in AGENT_SPECS {
            let automaton = automaton::parse_automaton(ioa_source).unwrap();
            assert_eq!(
                automaton.automaton.name, *entity_type,
                "Agent spec name mismatch: expected {entity_type}, got {}",
                automaton.automaton.name
            );
        }
    }

    #[test]
    fn test_agent_specs_verify() {
        for (entity_type, ioa_source) in AGENT_SPECS {
            let cascade = VerificationCascade::from_ioa(ioa_source)
                .with_sim_seeds(3)
                .with_prop_test_cases(50);
            let result = cascade.run();
            assert!(
                result.all_passed,
                "Agent spec {} failed verification",
                entity_type
            );
        }
    }

    #[test]
    fn test_bootstrap_agent_specs_registers_tenant() {
        let state = PlatformState::new(None);
        bootstrap_agent_specs(&state, "test-agent");
        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("test-agent");
        assert!(registry.get_tenant(&tenant).is_some());
        assert!(registry.get_table(&tenant, "Agent").is_some());
        assert!(registry.get_table(&tenant, "AgentType").is_some());
        assert!(registry.get_table(&tenant, "Plan").is_some());
        assert!(registry.get_table(&tenant, "Task").is_some());
        assert!(registry.get_table(&tenant, "ToolCall").is_some());
    }

    #[test]
    fn test_agent_specs_count() {
        assert_eq!(AGENT_SPECS.len(), 8);
    }
}
