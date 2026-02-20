//! Interactive demo: shows IOA -> TransitionTable -> Cascade -> DST internals.
//!
//! Run with: cargo test -p ecommerce-reference --test interactive_demo -- --nocapture

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{FaultConfig, SimActorSystem, SimActorSystemConfig};
use temper_server::entity_actor::sim_handler::EntityActorHandler;
use temper_spec::automaton;
use temper_verify::cascade::VerificationCascade;

const ORDER_IOA: &str = include_str!("../specs/order.ioa.toml");

#[test]
fn interactive_full_pipeline() {
    println!();
    println!("============================================================");
    println!("  TEMPER VERIFICATION PIPELINE — INTERACTIVE DEMO");
    println!("============================================================");

    // ── Stage 1: Parse IOA TOML ──────────────────────────────────────
    println!("\n--- STAGE 1: Parse IOA TOML ---\n");
    let automaton = automaton::parse_automaton(ORDER_IOA).expect("failed to parse");

    println!("  Entity:   {}", automaton.automaton.name);
    println!("  States:   {:?}", automaton.automaton.states);
    println!("  Initial:  {}", automaton.automaton.initial);
    println!("  State vars:");
    for sv in &automaton.state {
        println!(
            "    - {} (type={}, initial={})",
            sv.name, sv.var_type, sv.initial
        );
    }
    println!("  Actions:  {} total", automaton.actions.len());
    for a in &automaton.actions {
        let guard_str = if a.guard.is_empty() {
            "none".to_string()
        } else {
            format!("{:?}", a.guard)
        };
        println!(
            "    {:20} kind={:8} from={:<30} to={:<15} guard={}",
            a.name,
            a.kind,
            format!("{:?}", a.from),
            a.to.as_deref().unwrap_or("-"),
            guard_str,
        );
    }
    println!("  Invariants: {}", automaton.invariants.len());
    for inv in &automaton.invariants {
        println!(
            "    {:25} when={:<50} assert={}",
            inv.name,
            format!("{:?}", inv.when),
            inv.assert
        );
    }

    // ── Stage 2: Build TransitionTable ──────────────────────────────
    println!("\n--- STAGE 2: Build TransitionTable (temper-jit) ---\n");
    let table = TransitionTable::from_ioa_source(ORDER_IOA);

    println!("  Entity:   {}", table.entity_name);
    println!("  States:   {:?}", table.states);
    println!("  Initial:  {}", table.initial_state);
    println!("  Rules:    {} total", table.rules.len());
    for rule in &table.rules {
        println!(
            "    {:20} from={:<30} to={:<15} guard={:?}",
            rule.name,
            format!("{:?}", rule.from_states),
            rule.to_state.as_deref().unwrap_or("-"),
            rule.guard,
        );
        println!("    {:20} effects={:?}", "", rule.effects);
    }

    // ── Stage 3: Test guard evaluation ──────────────────────────────
    println!("\n--- STAGE 3: Guard Evaluation Examples ---\n");

    use std::collections::BTreeMap;
    use temper_jit::table::EvalContext;

    // SubmitOrder with 0 items -> should FAIL
    let ctx_empty = EvalContext {
        counters: BTreeMap::from([("items".to_string(), 0)]),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
    };
    let result = table.evaluate_ctx("Draft", &ctx_empty, "SubmitOrder");
    println!("  SubmitOrder from Draft, items=0:");
    match &result {
        Some(r) => println!("    success={}, new_state={}", r.success, r.new_state),
        None => println!("    action not found"),
    }

    // SubmitOrder with 2 items -> should PASS
    let ctx_items = EvalContext {
        counters: BTreeMap::from([("items".to_string(), 2)]),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
    };
    let result = table.evaluate_ctx("Draft", &ctx_items, "SubmitOrder");
    println!("  SubmitOrder from Draft, items=2:");
    match &result {
        Some(r) => println!("    success={}, new_state={}", r.success, r.new_state),
        None => println!("    action not found"),
    }

    // CancelOrder from Shipped -> should FAIL (not in from_states)
    let result = table.evaluate_ctx("Shipped", &ctx_items, "CancelOrder");
    println!("  CancelOrder from Shipped:");
    match &result {
        Some(r) => println!("    success={}, new_state={}", r.success, r.new_state),
        None => println!("    action not found"),
    }

    // CancelOrder from Draft -> should PASS
    let result = table.evaluate_ctx("Draft", &ctx_empty, "CancelOrder");
    println!("  CancelOrder from Draft:");
    match &result {
        Some(r) => println!("    success={}, new_state={}", r.success, r.new_state),
        None => println!("    action not found"),
    }

    // ── Stage 4: Verification Cascade ───────────────────────────────
    println!("\n--- STAGE 4: Verification Cascade (4 levels) ---\n");
    let cascade = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(5)
        .with_prop_test_cases(500);

    let cascade_result = cascade.run();

    for level in &cascade_result.levels {
        let icon = if level.passed { "PASS" } else { "FAIL" };
        println!("  [{}] {}", icon, level.summary);

        // Show L0 SMT details
        if let Some(ref smt) = level.smt {
            let dead: Vec<&str> = smt
                .guard_satisfiability
                .iter()
                .filter(|(_, sat)| !sat)
                .map(|(n, _)| n.as_str())
                .collect();
            let non_ind: Vec<&str> = smt
                .inductive_invariants
                .iter()
                .filter(|(_, ind)| !ind)
                .map(|(n, _)| n.as_str())
                .collect();
            if !dead.is_empty() {
                println!("         Dead guards: {:?}", dead);
            }
            if !non_ind.is_empty() {
                println!("         Non-inductive: {:?}", non_ind);
            }
            if !smt.unreachable_states.is_empty() {
                println!("         Unreachable: {:?}", smt.unreachable_states);
            }
        }

        // Show L1 details
        if let Some(ref v) = level.verification {
            println!("         States explored: {}", v.states_explored);
            println!("         Properties hold: {}", v.all_properties_hold);
            if !v.counterexamples.is_empty() {
                for ce in &v.counterexamples {
                    println!("         COUNTEREXAMPLE: {}", ce.property);
                }
            }
        }

        // Show L2 details
        if let Some(ref s) = level.simulation {
            println!("         Transitions: {}", s.total_transitions);
            println!("         Dropped msgs: {}", s.total_dropped);
            println!("         Violations: {}", s.violations.len());
        }

        // Show L3 details
        if let Some(ref p) = level.prop_test {
            println!("         Cases run: {}", p.total_cases);
            if let Some(ref f) = p.failure {
                println!(
                    "         FAILURE: invariant '{}' after {} actions",
                    f.invariant,
                    f.action_sequence.len()
                );
            }
        }
    }

    println!(
        "\n  Overall: {}",
        if cascade_result.all_passed {
            "ALL PASSED"
        } else {
            "FAILED"
        }
    );

    // ── Stage 5: DST — Scripted scenario ────────────────────────────
    println!("\n--- STAGE 5: DST -- Scripted Scenario (seed=42) ---\n");

    let config = SimActorSystemConfig {
        seed: 42,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);
    let handler =
        EntityActorHandler::new("Order", "ord-1", Arc::new(table)).with_ioa_invariants(ORDER_IOA);
    sim.register_actor("ord-1", Box::new(handler));

    let actions = [
        ("AddItem", "{}"),
        ("AddItem", "{}"),
        ("SubmitOrder", "{}"),
        ("ConfirmOrder", "{}"),
        ("ProcessOrder", "{}"),
        ("ShipOrder", "{}"),
        ("DeliverOrder", "{}"),
    ];

    for (action, params) in &actions {
        let before = sim.status("ord-1");
        match sim.step("ord-1", action, params) {
            Ok(_result) => {
                let after = sim.status("ord-1");
                println!("  {:<12} --[ {:<16} ]--> {}", before, action, after);
            }
            Err(e) => {
                println!("  {:<12} --[ {:<16} ]--> REJECTED: {}", before, action, e);
            }
        }
    }

    println!("\n  Final status: {}", sim.status("ord-1"));
    println!("  Violations:   {}", sim.violations().len());

    // ── Stage 6: DST — Random exploration with faults ───────────────
    println!("\n--- STAGE 6: DST -- Random Exploration (seed=1337, heavy faults) ---\n");

    let config = SimActorSystemConfig {
        seed: 1337,
        max_ticks: 300,
        faults: FaultConfig::heavy(),
        max_actions_per_actor: 25,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new(
            "Order",
            &format!("ord-{}", i),
            Arc::new(TransitionTable::from_ioa_source(ORDER_IOA)),
        )
        .with_ioa_invariants(ORDER_IOA);
        sim.register_actor(&format!("ord-{}", i), Box::new(handler));
    }

    let result = sim.run_random();

    println!("  Seed:             {}", result.seed);
    println!("  Transitions:      {}", result.transitions);
    println!("  Messages sent:    {}", result.messages);
    println!("  Messages dropped: {}", result.dropped);
    println!("  Invariants held:  {}", result.all_invariants_held);
    println!("  Violations:       {}", result.violations.len());
    println!("  Actor final states:");
    for (id, status, items, events) in &result.actor_states {
        println!(
            "    {}: status={}, items={}, events={}",
            id, status, items, events
        );
    }

    // ── Stage 7: Determinism proof ──────────────────────────────────
    println!("\n--- STAGE 7: Determinism Proof (seed=42, 5 runs) ---\n");

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
        let handler = EntityActorHandler::new(
            "Order",
            "ord-1",
            Arc::new(TransitionTable::from_ioa_source(ORDER_IOA)),
        )
        .with_ioa_invariants(ORDER_IOA);
        sim.register_actor("ord-1", Box::new(handler));

        let result = sim.run_random();
        let states = result.actor_states.clone();

        if let Some(ref r) = reference {
            let matches = &states == r;
            if !matches {
                all_match = false;
            }
            println!(
                "  Run {}: {:?}  {}",
                run,
                states,
                if matches {
                    "== reference"
                } else {
                    "!= MISMATCH!"
                }
            );
        } else {
            println!("  Run {}: {:?}  (reference)", run, states);
            reference = Some(states);
        }
    }
    println!(
        "\n  Determinism: {}",
        if all_match {
            "PROVEN (all runs bit-exact identical)"
        } else {
            "VIOLATED!"
        }
    );

    println!();
    println!("============================================================");
    println!("  DEMO COMPLETE — all stages exercised real code paths");
    println!("============================================================");
    println!();
}
