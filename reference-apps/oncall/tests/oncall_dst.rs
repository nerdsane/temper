//! Deterministic Simulation Tests for on-call entities.
//!
//! These DST tests exercise the Page, EscalationPolicy, Remediation, and
//! Postmortem specs through the SimActorSystem with:
//!
//! - **Scripted scenarios**: exact action sequences with state assertions
//! - **Random exploration**: seed-controlled random walks with fault injection
//! - **Determinism proofs**: bit-exact replay across multiple runs
//! - **Multi-entity scenarios**: all 4 entity types interacting together
//! - **Multi-seed sweeps**: bulk exploration across many seeds

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{FaultConfig, SimActorSystem, SimActorSystemConfig};
use temper_server::entity_actor::sim_handler::EntityActorHandler;

const PAGE_IOA: &str = include_str!("../specs/page.ioa.toml");
const ESCALATION_POLICY_IOA: &str = include_str!("../specs/escalation_policy.ioa.toml");
const REMEDIATION_IOA: &str = include_str!("../specs/remediation.ioa.toml");
const POSTMORTEM_IOA: &str = include_str!("../specs/postmortem.ioa.toml");

fn page_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(PAGE_IOA))
}

fn escalation_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(ESCALATION_POLICY_IOA))
}

fn remediation_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(REMEDIATION_IOA))
}

fn postmortem_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(POSTMORTEM_IOA))
}

// =========================================================================
// SCRIPTED SCENARIOS — Page Lifecycle
// =========================================================================

#[test]
fn page_starts_triggered() {
    let config = SimActorSystemConfig {
        seed: 1,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler =
        EntityActorHandler::new("Page", "page-1", page_table()).with_ioa_invariants(PAGE_IOA);
    sim.register_actor("page-1", Box::new(handler));

    sim.assert_status("page-1", "Triggered");
}

#[test]
fn page_assign_then_investigate() {
    let config = SimActorSystemConfig {
        seed: 2,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler =
        EntityActorHandler::new("Page", "page-1", page_table()).with_ioa_invariants(PAGE_IOA);
    sim.register_actor("page-1", Box::new(handler));

    sim.step("page-1", "AssignAgent", "{}").unwrap();
    sim.assert_status("page-1", "Triggered");

    sim.step("page-1", "StartInvestigation", "{}").unwrap();
    sim.assert_status("page-1", "Investigating");

    sim.assert_event_count("page-1", 2);
    assert!(!sim.has_violations());
}

#[test]
fn page_full_lifecycle() {
    let config = SimActorSystemConfig {
        seed: 3,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler =
        EntityActorHandler::new("Page", "page-1", page_table()).with_ioa_invariants(PAGE_IOA);
    sim.register_actor("page-1", Box::new(handler));

    // Triggered → AssignAgent → StartInvestigation → StartRemediation → Resolve
    sim.step("page-1", "AssignAgent", "{}").unwrap();
    sim.step("page-1", "StartInvestigation", "{}").unwrap();
    sim.assert_status("page-1", "Investigating");

    sim.step("page-1", "StartRemediation", "{}").unwrap();
    sim.assert_status("page-1", "Remediated");

    sim.step("page-1", "Resolve", "{}").unwrap();
    sim.assert_status("page-1", "Resolved");

    sim.assert_event_count("page-1", 4);
    assert!(!sim.has_violations());
}

#[test]
fn page_escalation_flow() {
    let config = SimActorSystemConfig {
        seed: 4,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler =
        EntityActorHandler::new("Page", "page-1", page_table()).with_ioa_invariants(PAGE_IOA);
    sim.register_actor("page-1", Box::new(handler));

    // Assign → Investigate → Escalate (tier 1) → Reassign → Escalate (tier 2)
    sim.step("page-1", "AssignAgent", "{}").unwrap();
    sim.step("page-1", "StartInvestigation", "{}").unwrap();
    sim.step("page-1", "Escalate", "{}").unwrap();
    sim.assert_status("page-1", "Escalated");

    sim.step("page-1", "ReassignAgent", "{}").unwrap();
    sim.assert_status("page-1", "Investigating");

    sim.step("page-1", "Escalate", "{}").unwrap();
    sim.assert_status("page-1", "Escalated");

    assert!(!sim.has_violations());
}

#[test]
fn page_auto_resolve() {
    let config = SimActorSystemConfig {
        seed: 5,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler =
        EntityActorHandler::new("Page", "page-1", page_table()).with_ioa_invariants(PAGE_IOA);
    sim.register_actor("page-1", Box::new(handler));

    sim.step("page-1", "AutoResolve", "{}").unwrap();
    sim.assert_status("page-1", "Resolved");

    // Resolved is terminal
    let result = sim.step("page-1", "AssignAgent", "{}");
    assert!(
        result.is_err(),
        "AssignAgent should fail from Resolved state"
    );

    assert!(!sim.has_violations());
}

#[test]
fn page_cannot_investigate_without_agent() {
    let config = SimActorSystemConfig {
        seed: 6,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler =
        EntityActorHandler::new("Page", "page-1", page_table()).with_ioa_invariants(PAGE_IOA);
    sim.register_actor("page-1", Box::new(handler));

    // StartInvestigation without AssignAgent should fail (guard: is_true agent_assigned)
    let result = sim.step("page-1", "StartInvestigation", "{}");
    assert!(
        result.is_err(),
        "StartInvestigation should fail without agent assigned"
    );
    sim.assert_status("page-1", "Triggered");
}

#[test]
fn page_escalation_increments_tier() {
    let config = SimActorSystemConfig {
        seed: 7,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler =
        EntityActorHandler::new("Page", "page-1", page_table()).with_ioa_invariants(PAGE_IOA);
    sim.register_actor("page-1", Box::new(handler));

    sim.step("page-1", "AssignAgent", "{}").unwrap();
    sim.step("page-1", "StartInvestigation", "{}").unwrap();

    // Each escalation increments the tier counter
    sim.step("page-1", "Escalate", "{}").unwrap();
    sim.assert_status("page-1", "Escalated");

    // EscalationTracked invariant: escalation_tier > 0
    assert!(!sim.has_violations());

    // Reassign and escalate again
    sim.step("page-1", "ReassignAgent", "{}").unwrap();
    sim.step("page-1", "Timeout", "{}").unwrap();
    sim.assert_status("page-1", "Escalated");

    assert!(!sim.has_violations());
}

// =========================================================================
// SCRIPTED SCENARIOS — Remediation Lifecycle
// =========================================================================

#[test]
fn remediation_approve_execute_succeed() {
    let config = SimActorSystemConfig {
        seed: 10,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Remediation", "rem-1", remediation_table())
        .with_ioa_invariants(REMEDIATION_IOA);
    sim.register_actor("rem-1", Box::new(handler));

    sim.assert_status("rem-1", "Proposed");

    sim.step("rem-1", "Approve", "{}").unwrap();
    sim.assert_status("rem-1", "Approved");

    sim.step("rem-1", "Execute", "{}").unwrap();
    sim.assert_status("rem-1", "Executing");

    sim.step("rem-1", "Succeed", "{}").unwrap();
    sim.assert_status("rem-1", "Succeeded");

    // Succeeded is terminal
    let result = sim.step("rem-1", "Retry", "{}");
    assert!(result.is_err(), "Retry should fail from Succeeded state");

    assert!(!sim.has_violations());
}

#[test]
fn remediation_reject_is_final() {
    let config = SimActorSystemConfig {
        seed: 11,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Remediation", "rem-1", remediation_table())
        .with_ioa_invariants(REMEDIATION_IOA);
    sim.register_actor("rem-1", Box::new(handler));

    sim.step("rem-1", "Reject", "{}").unwrap();
    sim.assert_status("rem-1", "Rejected");

    // Rejected is terminal
    let result = sim.step("rem-1", "Approve", "{}");
    assert!(result.is_err(), "Approve should fail from Rejected state");

    assert!(!sim.has_violations());
}

#[test]
fn remediation_retry_after_failure() {
    let config = SimActorSystemConfig {
        seed: 12,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Remediation", "rem-1", remediation_table())
        .with_ioa_invariants(REMEDIATION_IOA);
    sim.register_actor("rem-1", Box::new(handler));

    sim.step("rem-1", "AutoApprove", "{}").unwrap();
    sim.step("rem-1", "Execute", "{}").unwrap();
    sim.step("rem-1", "Fail", "{}").unwrap();
    sim.assert_status("rem-1", "Failed");

    // Retry → back to Approved
    sim.step("rem-1", "Retry", "{}").unwrap();
    sim.assert_status("rem-1", "Approved");

    // Execute again → succeed
    sim.step("rem-1", "Execute", "{}").unwrap();
    sim.step("rem-1", "Succeed", "{}").unwrap();
    sim.assert_status("rem-1", "Succeeded");

    assert!(!sim.has_violations());
}

#[test]
fn remediation_approval_required_for_execution() {
    let config = SimActorSystemConfig {
        seed: 13,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Remediation", "rem-1", remediation_table())
        .with_ioa_invariants(REMEDIATION_IOA);
    sim.register_actor("rem-1", Box::new(handler));

    // Execute from Proposed should fail (not in from_states)
    let result = sim.step("rem-1", "Execute", "{}");
    assert!(result.is_err(), "Execute should fail from Proposed state");
    sim.assert_status("rem-1", "Proposed");
}

// =========================================================================
// SCRIPTED SCENARIOS — Postmortem Lifecycle
// =========================================================================

#[test]
fn postmortem_requires_root_cause() {
    let config = SimActorSystemConfig {
        seed: 20,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Postmortem", "pm-1", postmortem_table())
        .with_ioa_invariants(POSTMORTEM_IOA);
    sim.register_actor("pm-1", Box::new(handler));

    sim.assert_status("pm-1", "Draft");

    // SubmitForReview without root cause should fail (guard: is_true has_root_cause)
    let result = sim.step("pm-1", "SubmitForReview", "{}");
    assert!(
        result.is_err(),
        "SubmitForReview should fail without root cause"
    );
    sim.assert_status("pm-1", "Draft");
}

#[test]
fn postmortem_revision_cycle() {
    let config = SimActorSystemConfig {
        seed: 21,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Postmortem", "pm-1", postmortem_table())
        .with_ioa_invariants(POSTMORTEM_IOA);
    sim.register_actor("pm-1", Box::new(handler));

    sim.step("pm-1", "AddRootCause", "{}").unwrap();
    sim.step("pm-1", "SubmitForReview", "{}").unwrap();
    sim.assert_status("pm-1", "InReview");

    // Request revision → back to Draft (but has_root_cause reset by RequestRevision)
    sim.step("pm-1", "RequestRevision", "{}").unwrap();
    sim.assert_status("pm-1", "Draft");

    // Need to add root cause again before re-submitting
    sim.step("pm-1", "AddRootCause", "{}").unwrap();
    sim.step("pm-1", "SubmitForReview", "{}").unwrap();
    sim.assert_status("pm-1", "InReview");

    assert!(!sim.has_violations());
}

#[test]
fn postmortem_full_lifecycle() {
    let config = SimActorSystemConfig {
        seed: 22,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Postmortem", "pm-1", postmortem_table())
        .with_ioa_invariants(POSTMORTEM_IOA);
    sim.register_actor("pm-1", Box::new(handler));

    sim.step("pm-1", "AddRootCause", "{}").unwrap();
    sim.step("pm-1", "SubmitForReview", "{}").unwrap();
    sim.step("pm-1", "ApprovePostmortem", "{}").unwrap();
    sim.assert_status("pm-1", "Approved");

    sim.step("pm-1", "Publish", "{}").unwrap();
    sim.assert_status("pm-1", "Published");

    // Published is terminal
    let result = sim.step("pm-1", "AddRootCause", "{}");
    assert!(
        result.is_err(),
        "AddRootCause should fail from Published state"
    );

    assert!(!sim.has_violations());
}

// =========================================================================
// RANDOM EXPLORATION — light faults, single entity type
// =========================================================================

#[test]
fn random_page_light_faults() {
    let config = SimActorSystemConfig {
        seed: 42,
        max_ticks: 200,
        faults: FaultConfig::light(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let actor_id = format!("page-{i}");
        let handler = EntityActorHandler::new("Page", actor_id.clone(), page_table())
            .with_ioa_invariants(PAGE_IOA);
        sim.register_actor(&actor_id, Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Random page exploration found invariant violations: {:?}",
        result.violations
    );
    assert!(
        result.transitions > 0,
        "Should have at least one transition"
    );
}

// =========================================================================
// RANDOM EXPLORATION — heavy faults, all entity types
// =========================================================================

#[test]
fn random_all_entities_heavy_faults() {
    let config = SimActorSystemConfig {
        seed: 1337,
        max_ticks: 500,
        faults: FaultConfig::heavy(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    sim.register_actor(
        "page-1",
        Box::new(
            EntityActorHandler::new("Page", "page-1", page_table()).with_ioa_invariants(PAGE_IOA),
        ),
    );
    sim.register_actor(
        "esc-1",
        Box::new(
            EntityActorHandler::new("EscalationPolicy", "esc-1", escalation_table())
                .with_ioa_invariants(ESCALATION_POLICY_IOA),
        ),
    );
    sim.register_actor(
        "rem-1",
        Box::new(
            EntityActorHandler::new("Remediation", "rem-1", remediation_table())
                .with_ioa_invariants(REMEDIATION_IOA),
        ),
    );
    sim.register_actor(
        "pm-1",
        Box::new(
            EntityActorHandler::new("Postmortem", "pm-1", postmortem_table())
                .with_ioa_invariants(POSTMORTEM_IOA),
        ),
    );

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Even heavy faults should not break invariants: {:?}",
        result.violations
    );
}

// =========================================================================
// MULTI-SEED SWEEP — bulk exploration
// =========================================================================

#[test]
fn random_multi_seed_sweep() {
    for seed in 0..20 {
        let config = SimActorSystemConfig {
            seed,
            max_ticks: 100,
            faults: FaultConfig::light(),
            max_actions_per_actor: 20,
        };
        let mut sim = SimActorSystem::new(config);

        sim.register_actor(
            "page",
            Box::new(
                EntityActorHandler::new("Page", "page", page_table()).with_ioa_invariants(PAGE_IOA),
            ),
        );
        sim.register_actor(
            "esc",
            Box::new(
                EntityActorHandler::new("EscalationPolicy", "esc", escalation_table())
                    .with_ioa_invariants(ESCALATION_POLICY_IOA),
            ),
        );
        sim.register_actor(
            "rem",
            Box::new(
                EntityActorHandler::new("Remediation", "rem", remediation_table())
                    .with_ioa_invariants(REMEDIATION_IOA),
            ),
        );
        sim.register_actor(
            "pm",
            Box::new(
                EntityActorHandler::new("Postmortem", "pm", postmortem_table())
                    .with_ioa_invariants(POSTMORTEM_IOA),
            ),
        );

        let result = sim.run_random();
        assert!(
            result.all_invariants_held,
            "Seed {seed} found invariant violations: {:?}",
            result.violations
        );
    }
}

// =========================================================================
// DETERMINISM PROOFS — same seed = bit-exact same outcome
// =========================================================================

fn run_determinism_trial(seed: u64) -> Vec<(String, String, usize, usize)> {
    let config = SimActorSystemConfig {
        seed,
        max_ticks: 300,
        faults: FaultConfig::light(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    sim.register_actor(
        "page-1",
        Box::new(
            EntityActorHandler::new("Page", "page-1", page_table()).with_ioa_invariants(PAGE_IOA),
        ),
    );
    sim.register_actor(
        "esc-1",
        Box::new(
            EntityActorHandler::new("EscalationPolicy", "esc-1", escalation_table())
                .with_ioa_invariants(ESCALATION_POLICY_IOA),
        ),
    );
    sim.register_actor(
        "rem-1",
        Box::new(
            EntityActorHandler::new("Remediation", "rem-1", remediation_table())
                .with_ioa_invariants(REMEDIATION_IOA),
        ),
    );
    sim.register_actor(
        "pm-1",
        Box::new(
            EntityActorHandler::new("Postmortem", "pm-1", postmortem_table())
                .with_ioa_invariants(POSTMORTEM_IOA),
        ),
    );

    let result = sim.run_random();
    assert!(result.all_invariants_held);
    result.actor_states
}

#[test]
fn determinism_proof_seed_42() {
    let reference = run_determinism_trial(42);
    for run in 1..10 {
        let trial = run_determinism_trial(42);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 42 must produce identical results"
        );
    }
}

#[test]
fn determinism_proof_seed_1337() {
    let reference = run_determinism_trial(1337);
    for run in 1..10 {
        let trial = run_determinism_trial(1337);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 1337 must produce identical results"
        );
    }
}
