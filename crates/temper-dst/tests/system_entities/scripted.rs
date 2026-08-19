use super::prelude::*;

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
    register_project(&mut sim, "proj-1");
    register_tenant(&mut sim, "tenant-prod");
    register_collaborator(&mut sim, "dev-alice");
    register_version(&mut sim, "v1");
    register_catalog_entry(&mut sim, "catalog-1");

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
