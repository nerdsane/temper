//! Interactive demo: full agent triage flow across all 4 on-call entities.
//!
//! Alert → Page → Assign Agent → Investigate → Propose Remediation → Approve →
//! Execute → Succeed → Resolve → Draft Postmortem → Add Root Cause → Review →
//! Approve → Publish.
//!
//! Run with: cargo test -p oncall-reference --test interactive_demo -- --nocapture

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{
    FaultConfig, SimActorSystem, SimActorSystemConfig,
};
use temper_server::entity_actor::sim_handler::EntityActorHandler;
use temper_spec::automaton;
use temper_verify::cascade::VerificationCascade;

const PAGE_IOA: &str = include_str!("../specs/page.ioa.toml");
const ESCALATION_POLICY_IOA: &str = include_str!("../specs/escalation_policy.ioa.toml");
const REMEDIATION_IOA: &str = include_str!("../specs/remediation.ioa.toml");
const POSTMORTEM_IOA: &str = include_str!("../specs/postmortem.ioa.toml");

#[test]
fn interactive_full_triage_pipeline() {
    println!();
    println!("============================================================");
    println!("  TEMPER ON-CALL — AGENT TRIAGE PIPELINE DEMO");
    println!("============================================================");

    // ── Stage 1: Parse all IOA specs ──────────────────────────────────
    println!("\n--- STAGE 1: Parse IOA Specs ---\n");

    for (name, source) in [
        ("Page", PAGE_IOA),
        ("EscalationPolicy", ESCALATION_POLICY_IOA),
        ("Remediation", REMEDIATION_IOA),
        ("Postmortem", POSTMORTEM_IOA),
    ] {
        let automaton = automaton::parse_automaton(source)
            .unwrap_or_else(|e| panic!("failed to parse {name}: {e}"));
        println!("  {:<20} states={:<3} actions={:<3} invariants={}",
            name,
            automaton.automaton.states.len(),
            automaton.actions.len(),
            automaton.invariants.len(),
        );
    }

    // ── Stage 2: Verification Cascade (Page) ─────────────────────────
    println!("\n--- STAGE 2: Verification Cascade (Page) ---\n");

    let cascade = VerificationCascade::from_ioa(PAGE_IOA)
        .with_sim_seeds(5)
        .with_prop_test_cases(500);
    let cascade_result = cascade.run();

    for level in &cascade_result.levels {
        let icon = if level.passed { "PASS" } else { "FAIL" };
        println!("  [{}] {}", icon, level.summary);
    }
    println!("\n  Overall: {}", if cascade_result.all_passed { "ALL PASSED" } else { "FAILED" });

    // ── Stage 3: Full agent triage — scripted multi-entity ───────────
    println!("\n--- STAGE 3: Agent Triage Flow (4 entities) ---\n");

    let config = SimActorSystemConfig { seed: 42, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let page = EntityActorHandler::new("Page", "page-1",
        Arc::new(TransitionTable::from_ioa_source(PAGE_IOA)))
        .with_ioa_invariants(PAGE_IOA);
    let esc = EntityActorHandler::new("EscalationPolicy", "esc-1",
        Arc::new(TransitionTable::from_ioa_source(ESCALATION_POLICY_IOA)))
        .with_ioa_invariants(ESCALATION_POLICY_IOA);
    let rem = EntityActorHandler::new("Remediation", "rem-1",
        Arc::new(TransitionTable::from_ioa_source(REMEDIATION_IOA)))
        .with_ioa_invariants(REMEDIATION_IOA);
    let pm = EntityActorHandler::new("Postmortem", "pm-1",
        Arc::new(TransitionTable::from_ioa_source(POSTMORTEM_IOA)))
        .with_ioa_invariants(POSTMORTEM_IOA);

    sim.register_actor("page-1", Box::new(page));
    sim.register_actor("esc-1", Box::new(esc));
    sim.register_actor("rem-1", Box::new(rem));
    sim.register_actor("pm-1", Box::new(pm));

    let steps: Vec<(&str, &str, &str)> = vec![
        // 1. Alert fires, activate escalation policy
        ("esc-1", "Activate", "Escalation policy activated"),
        // 2. Assign agent and start investigating
        ("page-1", "AssignAgent", "Agent assigned to page"),
        ("page-1", "StartInvestigation", "Investigation started"),
        // 3. Agent proposes remediation, human approves
        ("rem-1", "Approve", "Human approved remediation"),
        // 4. Execute remediation
        ("rem-1", "Execute", "Remediation executing"),
        ("rem-1", "Succeed", "Remediation succeeded"),
        // 5. Resolve the page
        ("page-1", "StartRemediation", "Page marked remediated"),
        ("page-1", "Resolve", "Page resolved"),
        // 6. Draft and publish postmortem
        ("pm-1", "AddRootCause", "Root cause added"),
        ("pm-1", "SubmitForReview", "Postmortem submitted for review"),
        ("pm-1", "ApprovePostmortem", "Postmortem approved"),
        ("pm-1", "Publish", "Postmortem published"),
    ];

    for (actor, action, description) in &steps {
        let before = sim.status(actor);
        match sim.step(actor, action, "{}") {
            Ok(_) => {
                let after = sim.status(actor);
                println!("  {:<8} {:<12} --[ {:<20} ]--> {:<12}  {}",
                    actor, before, action, after, description);
            }
            Err(e) => {
                println!("  {:<8} {:<12} --[ {:<20} ]--> REJECTED: {}",
                    actor, before, action, e);
            }
        }
    }

    println!("\n  Final states:");
    println!("    Page:             {}", sim.status("page-1"));
    println!("    EscalationPolicy: {}", sim.status("esc-1"));
    println!("    Remediation:      {}", sim.status("rem-1"));
    println!("    Postmortem:       {}", sim.status("pm-1"));
    println!("    Violations:       {}", sim.violations().len());

    assert!(!sim.has_violations(), "violations: {:?}", sim.violations());

    // ── Stage 4: Determinism proof ──────────────────────────────────
    println!("\n--- STAGE 4: Determinism Proof (seed=42, 5 runs) ---\n");

    let mut reference: Option<Vec<(String, String, usize, usize)>> = None;
    let mut all_match = true;

    for run in 0..5 {
        let config = SimActorSystemConfig {
            seed: 42,
            max_ticks: 200,
            faults: FaultConfig::light(),
            max_actions_per_actor: 20,
        };
        let mut sim = SimActorSystem::new(config);

        sim.register_actor("page-1", Box::new(
            EntityActorHandler::new("Page", "page-1",
                Arc::new(TransitionTable::from_ioa_source(PAGE_IOA)))
                .with_ioa_invariants(PAGE_IOA)));
        sim.register_actor("esc-1", Box::new(
            EntityActorHandler::new("EscalationPolicy", "esc-1",
                Arc::new(TransitionTable::from_ioa_source(ESCALATION_POLICY_IOA)))
                .with_ioa_invariants(ESCALATION_POLICY_IOA)));

        let result = sim.run_random();
        let states = result.actor_states.clone();

        if let Some(ref r) = reference {
            let matches = &states == r;
            if !matches { all_match = false; }
            println!("  Run {}: {:?}  {}",
                run, states,
                if matches { "== reference" } else { "!= MISMATCH!" });
        } else {
            println!("  Run {}: {:?}  (reference)", run, states);
            reference = Some(states);
        }
    }
    println!("\n  Determinism: {}",
        if all_match { "PROVEN (all runs bit-exact identical)" } else { "VIOLATED!" });

    println!();
    println!("============================================================");
    println!("  DEMO COMPLETE — full agent triage pipeline exercised");
    println!("============================================================");
    println!();
}
