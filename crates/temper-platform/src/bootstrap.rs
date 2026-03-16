//! System tenant bootstrap.
//!
//! Loads the platform's own entity specs (Project, Tenant, CatalogEntry,
//! Collaborator, Version), runs the verification cascade, and registers
//! them as the `temper-system` tenant. This is dogfooding: the platform
//! manages itself using its own framework.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_spec::automaton;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::{TursoEventStore, TursoSpecVerificationUpdate, spec_content_hash};
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
    ("Policy", POLICY_IOA),
];

/// Verify, parse, and register a set of IOA specs under a tenant.
///
/// Uses content-hash gating: if a spec's SHA-256 hash matches a previously
/// verified entry in `verified_cache`, the verification cascade is skipped.
/// This prevents the expensive Z3 + Stateright + proptest cascade from
/// running on every boot (which caused OOM on Railway's 512 MB containers).
///
/// Returns a list of `(entity_type, content_hash)` for all bootstrapped specs
/// so the caller can persist them to the backing store.
///
/// Panics if any spec fails to parse or verify (fatal startup error).
pub(crate) fn bootstrap_tenant_specs(
    state: &PlatformState,
    tenant: &str,
    csdl_source: &str,
    specs: &[(&str, &str)],
    merge: bool,
    label: &str,
    verified_cache: &BTreeMap<String, (String, bool)>,
) -> Vec<(String, String)> {
    tracing::info!(
        "Bootstrapping {label} specs for tenant '{tenant}' with {} entities",
        specs.len()
    );

    // Validate all specs parse.
    for (entity_type, ioa_source) in specs {
        automaton::parse_automaton(ioa_source)
            .unwrap_or_else(|e| panic!("{label} spec {entity_type} failed to parse: {e}"));
    }

    // Hash-gated verification: only run the cascade for specs whose
    // content has changed since the last successful verification.
    let mut spec_hashes = Vec::with_capacity(specs.len());
    for (entity_type, ioa_source) in specs {
        let hash = spec_content_hash(ioa_source);
        let already_verified = verified_cache
            .get(*entity_type)
            .is_some_and(|(cached_hash, verified)| *verified && cached_hash == &hash);

        if already_verified {
            tracing::info!(
                "Spec {entity_type} unchanged (hash={}…), skipping verification",
                &hash[..8]
            );
        } else {
            tracing::info!(
                "Spec {entity_type} needs verification (hash={}…), running cascade",
                &hash[..8]
            );
            let cascade = VerificationCascade::from_ioa(ioa_source)
                .with_sim_seeds(3)
                .with_prop_test_cases(20);
            let result = cascade.run();
            assert!(
                result.all_passed,
                "{label} spec {entity_type} failed verification cascade"
            );
        }
        spec_hashes.push((entity_type.to_string(), hash));
    }

    // Parse CSDL schema.
    let csdl =
        parse_csdl(csdl_source).unwrap_or_else(|e| panic!("{label} CSDL failed to parse: {e}"));

    // Register tenant and mark specs as pre-verified.
    let tenant_id = TenantId::new(tenant);
    {
        let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
        registry
            .try_register_tenant_with_reactions_and_constraints(
                tenant_id.clone(),
                csdl,
                csdl_source.to_string(),
                specs,
                Vec::new(),
                None,
                merge,
            )
            .unwrap_or_else(|e| panic!("failed to register {label} specs for '{tenant}': {e}"));
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

    spec_hashes
}

/// Bootstrap the system tenant.
///
/// Validates, verifies, and registers all temper-system entity specs.
/// Returns `(entity_type, content_hash)` pairs for persistence.
/// Panics if system specs fail to parse or verify (fatal startup error).
pub fn bootstrap_system_tenant(
    state: &PlatformState,
    verified_cache: &BTreeMap<String, (String, bool)>,
) -> Vec<(String, String)> {
    bootstrap_tenant_specs(
        state,
        SYSTEM_TENANT,
        SYSTEM_CSDL,
        SYSTEM_SPECS,
        false,
        "System",
        verified_cache,
    )
}

/// Bootstrap agent entity specs (Agent, Plan, Task, ToolCall) for a tenant.
///
/// Parses and verifies the agent IOA specs, then registers them under the
/// given tenant. Returns `(entity_type, content_hash)` pairs for persistence.
/// Panics if agent specs fail to parse or verify.
pub fn bootstrap_agent_specs(
    state: &PlatformState,
    tenant: &str,
    verified_cache: &BTreeMap<String, (String, bool)>,
) -> Vec<(String, String)> {
    bootstrap_tenant_specs(
        state,
        tenant,
        AGENT_CSDL,
        AGENT_SPECS,
        false,
        "Agent",
        verified_cache,
    )
}

/// Persist built-in spec hashes and verification status to Turso.
///
/// After bootstrap verifies specs (or skips via cache), this writes each
/// spec into the `specs` table with its content hash and marks it verified.
/// On subsequent boots, `load_verification_cache` finds these rows and
/// the cascade is skipped — preventing OOM on memory-constrained hosts.
///
/// Note: the upsert + mark-verified is two statements, not atomic.  If
/// the process crashes between them the spec row will have `verified=0`
/// and the cascade will re-run on next boot — safe, just slower.
pub(crate) async fn persist_bootstrap_verification(
    turso: &TursoEventStore,
    tenant: &str,
    specs: &[(&str, &str)],
    csdl_source: &str,
    hashes: &[(String, String)],
) {
    for (entity_type, content_hash) in hashes {
        // Find the IOA source for this entity type.
        let ioa_source = specs
            .iter()
            .find(|(et, _)| *et == entity_type)
            .map(|(_, src)| *src)
            .expect("hash returned for unknown entity type");

        // Upsert the spec row (preserves verification if hash unchanged).
        if let Err(e) = turso
            .upsert_spec(tenant, entity_type, ioa_source, csdl_source, content_hash)
            .await
        {
            tracing::warn!("Failed to persist bootstrap spec {tenant}/{entity_type}: {e}");
            continue;
        }

        // Mark as verified (bootstrap panics on failure, so all specs here passed).
        if let Err(e) = turso
            .persist_spec_verification(
                tenant,
                entity_type,
                TursoSpecVerificationUpdate {
                    status: "completed",
                    verified: true,
                    levels_passed: None,
                    levels_total: None,
                    verification_result_json: None,
                },
            )
            .await
        {
            tracing::warn!("Failed to persist verification status for {tenant}/{entity_type}: {e}");
        }
    }
}

/// Persist system tenant spec verification to Turso.
pub async fn persist_system_verification(turso: &TursoEventStore, hashes: &[(String, String)]) {
    persist_bootstrap_verification(turso, SYSTEM_TENANT, SYSTEM_SPECS, SYSTEM_CSDL, hashes).await;
}

/// Persist agent spec verification to Turso.
pub async fn persist_agent_verification(
    turso: &TursoEventStore,
    tenant: &str,
    hashes: &[(String, String)],
) {
    persist_bootstrap_verification(turso, tenant, AGENT_SPECS, AGENT_CSDL, hashes).await;
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

        bootstrap_system_tenant(&state, &BTreeMap::new());

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

        bootstrap_system_tenant(&state, &BTreeMap::new());

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
        bootstrap_agent_specs(&state, "test-agent", &BTreeMap::new());
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
        assert_eq!(AGENT_SPECS.len(), 7);
    }
}
