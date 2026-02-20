//! Deterministic Simulation Tests for platform system entities.
//!
//! These DST tests exercise the system entity specs (Project, Tenant,
//! CatalogEntry, Collaborator, Version) through the SimActorSystem with:
//!
//! - **Scripted scenarios**: exact action sequences with state assertions
//! - **Random exploration**: seed-controlled random walks with fault injection
//! - **Determinism proofs**: bit-exact replay across multiple runs
//! - **Multi-entity scenarios**: multiple system entities interacting together
//!
//! The system entities dogfood the same TransitionTable → EntityActorHandler
//! → SimActorSystem pipeline that user entities use. If these tests pass,
//! the platform's own control plane is provably correct.

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{FaultConfig, RunRecord, SimActorSystem, SimActorSystemConfig};
use temper_server::entity_actor::sim_handler::EntityActorHandler;

// Embed system specs at compile time (same as bootstrap.rs).
const PROJECT_IOA: &str = include_str!("../src/specs/Project.ioa.toml");
const TENANT_IOA: &str = include_str!("../src/specs/Tenant.ioa.toml");
const CATALOG_ENTRY_IOA: &str = include_str!("../src/specs/CatalogEntry.ioa.toml");
const COLLABORATOR_IOA: &str = include_str!("../src/specs/Collaborator.ioa.toml");
const VERSION_IOA: &str = include_str!("../src/specs/Version.ioa.toml");

fn project_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(PROJECT_IOA))
}

fn tenant_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(TENANT_IOA))
}

fn catalog_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(CATALOG_ENTRY_IOA))
}

fn collaborator_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(COLLABORATOR_IOA))
}

fn version_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(VERSION_IOA))
}

// =========================================================================
// SCRIPTED SCENARIOS — Project Lifecycle
// =========================================================================

#[test]
fn scripted_project_starts_in_created() {
    let config = SimActorSystemConfig {
        seed: 1,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Project", "proj-1", project_table())
        .with_ioa_invariants(PROJECT_IOA);
    sim.register_actor("proj-1", Box::new(handler));

    sim.assert_status("proj-1", "Created");
}

#[test]
fn scripted_project_full_lifecycle() {
    let config = SimActorSystemConfig {
        seed: 1,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Project", "proj-1", project_table())
        .with_ioa_invariants(PROJECT_IOA);
    sim.register_actor("proj-1", Box::new(handler));

    // Created → Building (UpdateSpecs)
    sim.step("proj-1", "UpdateSpecs", "{}").unwrap();
    sim.assert_status("proj-1", "Building");

    // Building → Verified (Verify, requires spec_count >= 1)
    sim.step("proj-1", "Verify", "{}").unwrap();
    sim.assert_status("proj-1", "Verified");

    // Verified → Archived (Archive)
    sim.step("proj-1", "Archive", "{}").unwrap();
    sim.assert_status("proj-1", "Archived");

    sim.assert_event_count("proj-1", 3);
    assert!(!sim.has_violations());
}

#[test]
fn scripted_project_cannot_verify_without_specs() {
    let config = SimActorSystemConfig {
        seed: 1,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Project", "proj-1", project_table())
        .with_ioa_invariants(PROJECT_IOA);
    sim.register_actor("proj-1", Box::new(handler));

    // Created → cannot Verify directly (needs spec_count >= 1, but also needs
    // to be in Building state)
    let result = sim.step("proj-1", "Verify", "{}");
    assert!(result.is_err(), "Verify should fail from Created state");
    sim.assert_status("proj-1", "Created");
}

#[test]
fn scripted_project_archive_from_any_state() {
    let config = SimActorSystemConfig {
        seed: 2,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    // Archive from Created
    let h1 = EntityActorHandler::new("Project", "p1", project_table());
    sim.register_actor("p1", Box::new(h1));
    sim.step("p1", "Archive", "{}").unwrap();
    sim.assert_status("p1", "Archived");

    // Archive from Building
    let h2 = EntityActorHandler::new("Project", "p2", project_table());
    sim.register_actor("p2", Box::new(h2));
    sim.step("p2", "UpdateSpecs", "{}").unwrap();
    sim.step("p2", "Archive", "{}").unwrap();
    sim.assert_status("p2", "Archived");

    // Archive from Verified
    let h3 = EntityActorHandler::new("Project", "p3", project_table());
    sim.register_actor("p3", Box::new(h3));
    sim.step("p3", "UpdateSpecs", "{}").unwrap();
    sim.step("p3", "Verify", "{}").unwrap();
    sim.step("p3", "Archive", "{}").unwrap();
    sim.assert_status("p3", "Archived");

    assert!(!sim.has_violations());
}

// =========================================================================
// SCRIPTED SCENARIOS — Tenant Lifecycle
// =========================================================================

#[test]
fn scripted_tenant_full_lifecycle() {
    let config = SimActorSystemConfig {
        seed: 10,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Tenant", "t-1", tenant_table());
    sim.register_actor("t-1", Box::new(handler));

    sim.assert_status("t-1", "Pending");

    // Pending → Active (Deploy)
    sim.step("t-1", "Deploy", "{}").unwrap();
    sim.assert_status("t-1", "Active");

    // Active → Suspended
    sim.step("t-1", "Suspend", "{}").unwrap();
    sim.assert_status("t-1", "Suspended");

    // Suspended → Active (Reactivate)
    sim.step("t-1", "Reactivate", "{}").unwrap();
    sim.assert_status("t-1", "Active");

    // Active → Archived
    sim.step("t-1", "Archive", "{}").unwrap();
    sim.assert_status("t-1", "Archived");

    sim.assert_event_count("t-1", 4);
    assert!(!sim.has_violations());
}

#[test]
fn scripted_tenant_suspend_resume_cycle() {
    let config = SimActorSystemConfig {
        seed: 11,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Tenant", "t-cycle", tenant_table());
    sim.register_actor("t-cycle", Box::new(handler));

    sim.step("t-cycle", "Deploy", "{}").unwrap();

    // Suspend → Reactivate 3 times
    for _ in 0..3 {
        sim.step("t-cycle", "Suspend", "{}").unwrap();
        sim.assert_status("t-cycle", "Suspended");
        sim.step("t-cycle", "Reactivate", "{}").unwrap();
        sim.assert_status("t-cycle", "Active");
    }

    sim.assert_event_count("t-cycle", 7); // Deploy + 3*(Suspend + Reactivate)
    assert!(!sim.has_violations());
}

#[test]
fn scripted_tenant_cannot_suspend_pending() {
    let config = SimActorSystemConfig {
        seed: 12,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Tenant", "t-err", tenant_table());
    sim.register_actor("t-err", Box::new(handler));

    let result = sim.step("t-err", "Suspend", "{}");
    assert!(result.is_err(), "Cannot suspend a Pending tenant");
    sim.assert_status("t-err", "Pending");
}

// =========================================================================
// SCRIPTED SCENARIOS — CatalogEntry Lifecycle
// =========================================================================

#[test]
fn scripted_catalog_publish_and_deprecate() {
    let config = SimActorSystemConfig {
        seed: 20,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("CatalogEntry", "cat-1", catalog_table());
    sim.register_actor("cat-1", Box::new(handler));

    sim.assert_status("cat-1", "Draft");

    sim.step("cat-1", "Publish", "{}").unwrap();
    sim.assert_status("cat-1", "Published");

    sim.step("cat-1", "Deprecate", "{}").unwrap();
    sim.assert_status("cat-1", "Deprecated");

    assert!(!sim.has_violations());
}

#[test]
fn scripted_catalog_fork_stays_published() {
    let config = SimActorSystemConfig {
        seed: 21,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("CatalogEntry", "cat-fork", catalog_table());
    sim.register_actor("cat-fork", Box::new(handler));

    sim.step("cat-fork", "Publish", "{}").unwrap();
    sim.step("cat-fork", "Fork", "{}").unwrap();
    // Fork is a non-transitioning action — stays Published
    sim.assert_status("cat-fork", "Published");
}

// =========================================================================
// SCRIPTED SCENARIOS — Collaborator Lifecycle
// =========================================================================

#[test]
fn scripted_collaborator_invite_accept_remove() {
    let config = SimActorSystemConfig {
        seed: 30,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Collaborator", "collab-1", collaborator_table());
    sim.register_actor("collab-1", Box::new(handler));

    sim.assert_status("collab-1", "Invited");

    sim.step("collab-1", "Accept", "{}").unwrap();
    sim.assert_status("collab-1", "Active");

    sim.step("collab-1", "ChangeRole", "{}").unwrap();
    sim.assert_status("collab-1", "Active"); // Non-transitioning

    sim.step("collab-1", "Remove", "{}").unwrap();
    sim.assert_status("collab-1", "Removed");

    sim.assert_event_count("collab-1", 3);
    assert!(!sim.has_violations());
}

#[test]
fn scripted_collaborator_remove_before_accept() {
    let config = SimActorSystemConfig {
        seed: 31,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Collaborator", "collab-2", collaborator_table());
    sim.register_actor("collab-2", Box::new(handler));

    // Remove directly from Invited
    sim.step("collab-2", "Remove", "{}").unwrap();
    sim.assert_status("collab-2", "Removed");
}

// =========================================================================
// SCRIPTED SCENARIOS — Version Lifecycle
// =========================================================================

#[test]
fn scripted_version_full_lifecycle() {
    let config = SimActorSystemConfig {
        seed: 40,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Version", "v-1", version_table());
    sim.register_actor("v-1", Box::new(handler));

    sim.assert_status("v-1", "Created");

    sim.step("v-1", "MarkDeployed", "{}").unwrap();
    sim.assert_status("v-1", "Deployed");

    sim.step("v-1", "Supersede", "{}").unwrap();
    sim.assert_status("v-1", "Superseded");

    sim.assert_event_count("v-1", 2);
    assert!(!sim.has_violations());
}

// =========================================================================
// MULTI-ENTITY SCENARIO — Platform control plane
// =========================================================================

#[test]
fn scripted_platform_control_plane_scenario() {
    let config = SimActorSystemConfig {
        seed: 100,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    // Register all system entity types
    let proj = EntityActorHandler::new("Project", "proj-1", project_table())
        .with_ioa_invariants(PROJECT_IOA);
    let tenant = EntityActorHandler::new("Tenant", "tenant-prod", tenant_table());
    let collab = EntityActorHandler::new("Collaborator", "dev-alice", collaborator_table());
    let ver = EntityActorHandler::new("Version", "v1", version_table());
    let cat = EntityActorHandler::new("CatalogEntry", "catalog-1", catalog_table());

    sim.register_actor("proj-1", Box::new(proj));
    sim.register_actor("tenant-prod", Box::new(tenant));
    sim.register_actor("dev-alice", Box::new(collab));
    sim.register_actor("v1", Box::new(ver));
    sim.register_actor("catalog-1", Box::new(cat));

    // 1. Alice accepts collaboration invite
    sim.step("dev-alice", "Accept", "{}").unwrap();
    sim.assert_status("dev-alice", "Active");

    // 2. Upload specs to project
    sim.step("proj-1", "UpdateSpecs", "{}").unwrap();
    sim.assert_status("proj-1", "Building");

    // 3. Verify project
    sim.step("proj-1", "Verify", "{}").unwrap();
    sim.assert_status("proj-1", "Verified");

    // 4. Create version
    sim.step("v1", "MarkDeployed", "{}").unwrap();
    sim.assert_status("v1", "Deployed");

    // 5. Deploy tenant
    sim.step("tenant-prod", "Deploy", "{}").unwrap();
    sim.assert_status("tenant-prod", "Active");

    // 6. Publish to catalog
    sim.step("catalog-1", "Publish", "{}").unwrap();
    sim.assert_status("catalog-1", "Published");

    // All 5 actors progressed without violations
    assert!(!sim.has_violations(), "violations: {:?}", sim.violations());
}

// =========================================================================
// RANDOM EXPLORATION — No-fault
// =========================================================================

#[test]
fn random_project_no_faults_seed_42() {
    let config = SimActorSystemConfig {
        seed: 42,
        max_ticks: 200,
        faults: FaultConfig::none(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new("Project", &format!("p-{i}"), project_table())
            .with_ioa_invariants(PROJECT_IOA);
        sim.register_actor(&format!("p-{i}"), Box::new(handler));
    }

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
    let config = SimActorSystemConfig {
        seed: 42,
        max_ticks: 200,
        faults: FaultConfig::none(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new("Tenant", &format!("t-{i}"), tenant_table());
        sim.register_actor(&format!("t-{i}"), Box::new(handler));
    }

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
    let config = SimActorSystemConfig {
        seed: 77,
        max_ticks: 500,
        faults: FaultConfig::none(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    sim.register_actor(
        "p1",
        Box::new(
            EntityActorHandler::new("Project", "p1", project_table())
                .with_ioa_invariants(PROJECT_IOA),
        ),
    );
    sim.register_actor(
        "t1",
        Box::new(EntityActorHandler::new("Tenant", "t1", tenant_table())),
    );
    sim.register_actor(
        "cat1",
        Box::new(EntityActorHandler::new(
            "CatalogEntry",
            "cat1",
            catalog_table(),
        )),
    );
    sim.register_actor(
        "col1",
        Box::new(EntityActorHandler::new(
            "Collaborator",
            "col1",
            collaborator_table(),
        )),
    );
    sim.register_actor(
        "v1",
        Box::new(EntityActorHandler::new("Version", "v1", version_table())),
    );

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
    let config = SimActorSystemConfig {
        seed: 99,
        max_ticks: 300,
        faults: FaultConfig::light(),
        max_actions_per_actor: 40,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new("Project", &format!("p-{i}"), project_table())
            .with_ioa_invariants(PROJECT_IOA);
        sim.register_actor(&format!("p-{i}"), Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Light faults should not break invariants: {:?}",
        result.violations
    );
}

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
        "p1",
        Box::new(
            EntityActorHandler::new("Project", "p1", project_table())
                .with_ioa_invariants(PROJECT_IOA),
        ),
    );
    sim.register_actor(
        "t1",
        Box::new(EntityActorHandler::new("Tenant", "t1", tenant_table())),
    );
    sim.register_actor(
        "cat1",
        Box::new(EntityActorHandler::new(
            "CatalogEntry",
            "cat1",
            catalog_table(),
        )),
    );
    sim.register_actor(
        "col1",
        Box::new(EntityActorHandler::new(
            "Collaborator",
            "col1",
            collaborator_table(),
        )),
    );
    sim.register_actor(
        "v1",
        Box::new(EntityActorHandler::new("Version", "v1", version_table())),
    );

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
    let config = SimActorSystemConfig {
        seed: 101,
        max_ticks: 300,
        faults: FaultConfig::light(),
        max_actions_per_actor: 40,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new("Tenant", &format!("t-{i}"), tenant_table());
        sim.register_actor(&format!("t-{i}"), Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Light faults should not break tenant invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_tenant_heavy_faults() {
    let config = SimActorSystemConfig {
        seed: 102,
        max_ticks: 500,
        faults: FaultConfig::heavy(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new("Tenant", &format!("t-{i}"), tenant_table());
        sim.register_actor(&format!("t-{i}"), Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Even heavy faults should not break tenant invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_project_heavy_faults() {
    let config = SimActorSystemConfig {
        seed: 103,
        max_ticks: 500,
        faults: FaultConfig::heavy(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new("Project", &format!("p-{i}"), project_table())
            .with_ioa_invariants(PROJECT_IOA);
        sim.register_actor(&format!("p-{i}"), Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Heavy faults should not break project invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_catalog_heavy_faults() {
    let config = SimActorSystemConfig {
        seed: 104,
        max_ticks: 500,
        faults: FaultConfig::heavy(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new("CatalogEntry", &format!("cat-{i}"), catalog_table());
        sim.register_actor(&format!("cat-{i}"), Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Heavy faults should not break catalog invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_collaborator_heavy_faults() {
    let config = SimActorSystemConfig {
        seed: 105,
        max_ticks: 500,
        faults: FaultConfig::heavy(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler =
            EntityActorHandler::new("Collaborator", &format!("col-{i}"), collaborator_table());
        sim.register_actor(&format!("col-{i}"), Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Heavy faults should not break collaborator invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_version_heavy_faults() {
    let config = SimActorSystemConfig {
        seed: 106,
        max_ticks: 500,
        faults: FaultConfig::heavy(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new("Version", &format!("v-{i}"), version_table());
        sim.register_actor(&format!("v-{i}"), Box::new(handler));
    }

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
        let config = SimActorSystemConfig {
            seed,
            max_ticks: 500,
            faults: FaultConfig::heavy(),
            max_actions_per_actor: 30,
        };
        let mut sim = SimActorSystem::new(config);

        sim.register_actor(
            "p1",
            Box::new(
                EntityActorHandler::new("Project", "p1", project_table())
                    .with_ioa_invariants(PROJECT_IOA),
            ),
        );
        sim.register_actor(
            "t1",
            Box::new(EntityActorHandler::new("Tenant", "t1", tenant_table())),
        );
        sim.register_actor(
            "cat1",
            Box::new(EntityActorHandler::new(
                "CatalogEntry",
                "cat1",
                catalog_table(),
            )),
        );
        sim.register_actor(
            "col1",
            Box::new(EntityActorHandler::new(
                "Collaborator",
                "col1",
                collaborator_table(),
            )),
        );
        sim.register_actor(
            "v1",
            Box::new(EntityActorHandler::new("Version", "v1", version_table())),
        );

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
        let config = SimActorSystemConfig {
            seed,
            max_ticks: 300,
            faults: FaultConfig::light(),
            max_actions_per_actor: 30,
        };
        let mut sim = SimActorSystem::new(config);

        sim.register_actor(
            "p1",
            Box::new(
                EntityActorHandler::new("Project", "p1", project_table())
                    .with_ioa_invariants(PROJECT_IOA),
            ),
        );
        sim.register_actor(
            "t1",
            Box::new(EntityActorHandler::new("Tenant", "t1", tenant_table())),
        );
        sim.register_actor(
            "cat1",
            Box::new(EntityActorHandler::new(
                "CatalogEntry",
                "cat1",
                catalog_table(),
            )),
        );
        sim.register_actor(
            "col1",
            Box::new(EntityActorHandler::new(
                "Collaborator",
                "col1",
                collaborator_table(),
            )),
        );
        sim.register_actor(
            "v1",
            Box::new(EntityActorHandler::new("Version", "v1", version_table())),
        );

        let result = sim.run_random();
        assert!(
            result.all_invariants_held,
            "Light faults seed {seed} found violations: {:?}",
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
        "p1",
        Box::new(
            EntityActorHandler::new("Project", "p1", project_table())
                .with_ioa_invariants(PROJECT_IOA),
        ),
    );
    sim.register_actor(
        "t1",
        Box::new(EntityActorHandler::new("Tenant", "t1", tenant_table())),
    );
    sim.register_actor(
        "cat1",
        Box::new(EntityActorHandler::new(
            "CatalogEntry",
            "cat1",
            catalog_table(),
        )),
    );
    sim.register_actor(
        "col1",
        Box::new(EntityActorHandler::new(
            "Collaborator",
            "col1",
            collaborator_table(),
        )),
    );
    sim.register_actor(
        "v1",
        Box::new(EntityActorHandler::new("Version", "v1", version_table())),
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
        let config = SimActorSystemConfig {
            seed,
            max_ticks: 100,
            faults: FaultConfig::light(),
            max_actions_per_actor: 20,
        };
        let mut sim = SimActorSystem::new(config);

        let handler = EntityActorHandler::new("Project", "p", project_table())
            .with_ioa_invariants(PROJECT_IOA);
        sim.register_actor("p", Box::new(handler));

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
        let config = SimActorSystemConfig {
            seed,
            max_ticks: 100,
            faults: FaultConfig::light(),
            max_actions_per_actor: 20,
        };
        let mut sim = SimActorSystem::new(config);

        let handler = EntityActorHandler::new("Tenant", "t", tenant_table());
        sim.register_actor("t", Box::new(handler));

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
    let config = SimActorSystemConfig {
        seed,
        max_ticks: 300,
        faults,
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    sim.register_actor(
        "p1",
        Box::new(
            EntityActorHandler::new("Project", "p1", project_table())
                .with_ioa_invariants(PROJECT_IOA),
        ),
    );
    sim.register_actor(
        "t1",
        Box::new(EntityActorHandler::new("Tenant", "t1", tenant_table())),
    );
    sim.register_actor(
        "cat1",
        Box::new(EntityActorHandler::new(
            "CatalogEntry",
            "cat1",
            catalog_table(),
        )),
    );
    sim.register_actor(
        "col1",
        Box::new(EntityActorHandler::new(
            "Collaborator",
            "col1",
            collaborator_table(),
        )),
    );
    sim.register_actor(
        "v1",
        Box::new(EntityActorHandler::new("Version", "v1", version_table())),
    );

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
