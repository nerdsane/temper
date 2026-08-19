use super::prelude::*;

// =========================================================================
// RANDOM EXPLORATION — No-fault
// =========================================================================

#[test]
fn random_project_no_faults_seed_42() {
    let mut sim = new_sim(42, 200, FaultConfig::none(), 30);

    register_projects(&mut sim, 3);

    let result = sim.run_random();

    assert!(
        result.all_invariants_held,
        "Random exploration found invariant violations: {:?}",
        result.violations
    );
    assert!(
        result.transitions > 0,
        "Should have at least one transition"
    );
}

#[test]
fn random_tenant_no_faults_seed_42() {
    let mut sim = new_sim(42, 200, FaultConfig::none(), 30);

    register_tenants(&mut sim, 3);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "violations: {:?}",
        result.violations
    );
    assert!(result.transitions > 0);
}

#[test]
fn random_all_system_entities_no_faults() {
    let mut sim = new_sim(77, 500, FaultConfig::none(), 30);

    register_all_system_entities(&mut sim);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "violations: {:?}",
        result.violations
    );
    assert!(result.transitions > 0);
}

// =========================================================================
// RANDOM EXPLORATION — With fault injection
// =========================================================================

#[test]
fn random_project_light_faults() {
    let mut sim = new_sim(99, 300, FaultConfig::light(), 40);

    register_projects(&mut sim, 3);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Light faults should not break invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_all_entities_heavy_faults() {
    let mut sim = new_sim(1337, 500, FaultConfig::heavy(), 30);

    register_all_system_entities(&mut sim);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Even heavy faults should not break invariants: {:?}",
        result.violations
    );
}

// =========================================================================
// RANDOM EXPLORATION — Per-entity heavy fault variants
// =========================================================================

#[test]
fn random_tenant_light_faults() {
    let mut sim = new_sim(101, 300, FaultConfig::light(), 40);

    register_tenants(&mut sim, 3);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Light faults should not break tenant invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_tenant_heavy_faults() {
    let mut sim = new_sim(102, 500, FaultConfig::heavy(), 30);

    register_tenants(&mut sim, 3);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Even heavy faults should not break tenant invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_project_heavy_faults() {
    let mut sim = new_sim(103, 500, FaultConfig::heavy(), 30);

    register_projects(&mut sim, 3);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Heavy faults should not break project invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_catalog_heavy_faults() {
    let mut sim = new_sim(104, 500, FaultConfig::heavy(), 30);

    register_catalog_entries(&mut sim, 3);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Heavy faults should not break catalog invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_collaborator_heavy_faults() {
    let mut sim = new_sim(105, 500, FaultConfig::heavy(), 30);

    register_collaborators(&mut sim, 3);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Heavy faults should not break collaborator invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_version_heavy_faults() {
    let mut sim = new_sim(106, 500, FaultConfig::heavy(), 30);

    register_versions(&mut sim, 3);

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Heavy faults should not break version invariants: {:?}",
        result.violations
    );
}

// =========================================================================
// RANDOM EXPLORATION — Multi-entity heavy fault sweep
// =========================================================================

#[test]
fn random_all_entities_heavy_faults_multi_seed() {
    for seed in [200, 201, 202, 203, 204] {
        let mut sim = new_sim(seed, 500, FaultConfig::heavy(), 30);

        register_all_system_entities(&mut sim);

        let result = sim.run_random();
        assert!(
            result.all_invariants_held,
            "Heavy faults seed {seed} found violations: {:?}",
            result.violations
        );
    }
}

#[test]
fn random_all_entities_light_faults_multi_seed() {
    for seed in [300, 301, 302, 303, 304] {
        let mut sim = new_sim(seed, 300, FaultConfig::light(), 30);

        register_all_system_entities(&mut sim);

        let result = sim.run_random();
        assert!(
            result.all_invariants_held,
            "Light faults seed {seed} found violations: {:?}",
            result.violations
        );
    }
}
