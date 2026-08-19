use super::prelude::*;

// =========================================================================
// DETERMINISM PROOFS — same seed = bit-exact same outcome
// =========================================================================

fn run_determinism_trial(seed: u64) -> Vec<(String, String, usize, usize)> {
    let mut sim = new_sim(seed, 300, FaultConfig::light(), 30);

    register_all_system_entities(&mut sim);

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

#[test]
fn determinism_proof_different_seeds_differ() {
    let s1 = run_determinism_trial(42);
    let s2 = run_determinism_trial(43);
    // Different seeds should (almost certainly) produce different outcomes
    assert_ne!(s1, s2, "Different seeds should produce different results");
}

// =========================================================================
// MULTI-SEED SWEEP — bulk exploration
// =========================================================================

#[test]
fn multi_seed_sweep_projects() {
    for seed in 0..20 {
        let mut sim = new_sim(seed, 100, FaultConfig::light(), 20);
        register_project(&mut sim, "p");

        let result = sim.run_random();
        assert!(
            result.all_invariants_held,
            "Seed {seed} found invariant violations: {:?}",
            result.violations
        );
    }
}

#[test]
fn multi_seed_sweep_tenants() {
    for seed in 0..20 {
        let mut sim = new_sim(seed, 100, FaultConfig::light(), 20);
        register_tenant(&mut sim, "t");

        let result = sim.run_random();
        assert!(
            result.all_invariants_held,
            "Seed {seed} found violations: {:?}",
            result.violations
        );
    }
}

// =========================================================================
// DETERMINISM CANARY — same seed MUST produce byte-exact same output
// =========================================================================

/// Run a full canary trial with all 5 system entity types and return the RunRecord.
fn run_canary_trial(seed: u64, faults: FaultConfig) -> RunRecord {
    let mut sim = new_sim(seed, 300, faults, 30);

    register_all_system_entities(&mut sim);

    let (result, record) = sim.run_random_recorded();
    assert!(
        result.all_invariants_held,
        "violations: {:?}",
        result.violations
    );
    record
}

#[test]
fn determinism_canary_comprehensive() {
    let seeds = [42, 1337, 0, 999, 7777, 12345];
    let fault_configs: Vec<(&str, FaultConfig)> = vec![
        ("none", FaultConfig::none()),
        ("light", FaultConfig::light()),
        ("heavy", FaultConfig::heavy()),
    ];

    for &seed in &seeds {
        for (fault_name, faults) in &fault_configs {
            let record_a = run_canary_trial(seed, faults.clone());
            let record_b = run_canary_trial(seed, faults.clone());

            assert_eq!(
                record_a, record_b,
                "Determinism canary FAILED: seed={seed}, faults={fault_name} \
                 produced different results on two runs"
            );

            assert!(
                !record_a.transitions.is_empty(),
                "Canary run was trivially empty: seed={seed}, faults={fault_name}"
            );
        }
    }
}

#[test]
fn determinism_canary_different_seeds_differ() {
    let record_42 = run_canary_trial(42, FaultConfig::none());
    let record_43 = run_canary_trial(43, FaultConfig::none());

    assert_ne!(
        record_42, record_43,
        "Different seeds (42 vs 43) should produce different run records"
    );
}
