//! Deterministic Simulation Tests for Crucible entities.
//!
//! These DST tests exercise the `Environment`, `EnvironmentAllowedHost`,
//! and `EnvironmentPackage` specs through `SimActorSystem` with:
//!
//! - **Scripted scenarios**: exact action sequences with state assertions.
//! - **Randomized exploration**: seed-controlled random walks with light faults.
//! - **Determinism proof**: two consecutive runs under the same seed must
//!   produce identical final state.
//!
//! Child entity specs have a single state and no transitions, so only the
//! `Environment` spec carries scripted lifecycle tests. The child specs are
//! exercised by the random walk (registration + zero-transition steady-state).

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{FaultConfig, SimActorSystem, SimActorSystemConfig};
use temper_server::entity_actor::sim_handler::EntityActorHandler;

const ENVIRONMENT_IOA: &str = include_str!("../specs/environment.ioa.toml");
const ALLOWED_HOST_IOA: &str = include_str!("../specs/environment_allowed_host.ioa.toml");
const PACKAGE_IOA: &str = include_str!("../specs/environment_package.ioa.toml");

fn environment_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(ENVIRONMENT_IOA))
}

fn allowed_host_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(ALLOWED_HOST_IOA))
}

fn package_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(PACKAGE_IOA))
}

// =========================================================================
// SCRIPTED SCENARIOS — Environment Lifecycle
// =========================================================================

#[test]
fn scripted_env_starts_active() {
    let config = SimActorSystemConfig {
        seed: 1,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Environment", "env-1", environment_table())
        .with_ioa_invariants(ENVIRONMENT_IOA);
    sim.register_actor("env-1", Box::new(handler));

    sim.assert_status("env-1", "Active");
}

#[test]
fn scripted_env_archive_transitions_to_archived() {
    let config = SimActorSystemConfig {
        seed: 2,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Environment", "env-1", environment_table())
        .with_ioa_invariants(ENVIRONMENT_IOA);
    sim.register_actor("env-1", Box::new(handler));

    sim.step("env-1", "ArchiveEnvironment", "{}").unwrap();
    sim.assert_status("env-1", "Archived");

    assert!(!sim.has_violations());
}

#[test]
fn scripted_env_archive_is_final() {
    let config = SimActorSystemConfig {
        seed: 3,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Environment", "env-1", environment_table())
        .with_ioa_invariants(ENVIRONMENT_IOA);
    sim.register_actor("env-1", Box::new(handler));

    sim.step("env-1", "ArchiveEnvironment", "{}").unwrap();
    sim.assert_status("env-1", "Archived");

    // Second archive must be rejected — Archived is terminal.
    let result = sim.step("env-1", "ArchiveEnvironment", "{}");
    assert!(
        result.is_err(),
        "ArchiveEnvironment should fail from Archived state"
    );

    assert!(!sim.has_violations());
}

// =========================================================================
// SCRIPTED SCENARIOS — Child entities start Active
// =========================================================================

#[test]
fn scripted_allowed_host_starts_active() {
    let config = SimActorSystemConfig {
        seed: 4,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("EnvironmentAllowedHost", "host-1", allowed_host_table())
        .with_ioa_invariants(ALLOWED_HOST_IOA);
    sim.register_actor("host-1", Box::new(handler));

    sim.assert_status("host-1", "Active");
}

#[test]
fn scripted_package_starts_active() {
    let config = SimActorSystemConfig {
        seed: 5,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("EnvironmentPackage", "pkg-1", package_table())
        .with_ioa_invariants(PACKAGE_IOA);
    sim.register_actor("pkg-1", Box::new(handler));

    sim.assert_status("pkg-1", "Active");
}

// =========================================================================
// RANDOM EXPLORATION
// =========================================================================

#[test]
fn random_all_entities_no_faults() {
    let config = SimActorSystemConfig {
        seed: 42,
        max_ticks: 200,
        faults: FaultConfig::none(),
        max_actions_per_actor: 20,
    };
    let mut sim = SimActorSystem::new(config);

    for (name, entity_type, table, ioa) in [
        ("env-1", "Environment", environment_table(), ENVIRONMENT_IOA),
        (
            "host-1",
            "EnvironmentAllowedHost",
            allowed_host_table(),
            ALLOWED_HOST_IOA,
        ),
        ("pkg-1", "EnvironmentPackage", package_table(), PACKAGE_IOA),
    ] {
        let handler = EntityActorHandler::new(entity_type, name, table).with_ioa_invariants(ioa);
        sim.register_actor(name, Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Random exploration found invariant violations: {:?}",
        result.violations
    );
}

// =========================================================================
// DETERMINISM PROOF — two runs under the same seed must match bit-exact
// =========================================================================

fn run_determinism_trial(seed: u64) -> Vec<(String, String, usize, usize)> {
    let config = SimActorSystemConfig {
        seed,
        max_ticks: 200,
        faults: FaultConfig::light(),
        max_actions_per_actor: 20,
    };
    let mut sim = SimActorSystem::new(config);

    for (name, entity_type, table, ioa) in [
        ("env-1", "Environment", environment_table(), ENVIRONMENT_IOA),
        (
            "host-1",
            "EnvironmentAllowedHost",
            allowed_host_table(),
            ALLOWED_HOST_IOA,
        ),
        ("pkg-1", "EnvironmentPackage", package_table(), PACKAGE_IOA),
    ] {
        let handler = EntityActorHandler::new(entity_type, name, table).with_ioa_invariants(ioa);
        sim.register_actor(name, Box::new(handler));
    }

    let result = sim.run_random();
    assert!(result.all_invariants_held);
    result.actor_states
}

#[test]
fn determinism_proof_seed_7() {
    let reference = run_determinism_trial(7);
    for run in 1..5 {
        let trial = run_determinism_trial(7);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 7 must produce identical results"
        );
    }
}

#[test]
fn determinism_proof_seed_1337() {
    let reference = run_determinism_trial(1337);
    for run in 1..5 {
        let trial = run_determinism_trial(1337);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 1337 must produce identical results"
        );
    }
}
