//! Production EntityActor tests for platform system entities.
//!
//! These tests exercise the **production code path**: real `EntityActor` instances
//! spawned in a real `ActorSystem`, receiving `EntityMsg::Action` via `ask()`.
//! This is the same code that runs in the live server — no simulation abstractions.
//!
//! Combined with the DST tests in `system_entity_dst.rs` (which prove invariant
//! correctness under fault injection), these tests verify the production wiring.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_server::{EntityActor, EntityMsg, EntityResponse};

mod common;

use common::specs::{
    SYSTEM_MODEL_CSDL_XML, catalog_table_rw, collaborator_table_rw, project_table_rw,
    tenant_table_rw, version_table_rw,
};

const TIMEOUT: Duration = Duration::from_secs(2);

// =========================================================================
// PROJECT — Production EntityActor
// =========================================================================

#[tokio::test]
async fn actor_project_starts_in_created() {
    let system = ActorSystem::new("test-project");
    let actor = EntityActor::new("Project", "p-1", project_table_rw(), serde_json::json!({}));
    let actor_ref = system.spawn(actor, "p-1");

    let r: EntityResponse = actor_ref.ask(EntityMsg::GetState, TIMEOUT).await.unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Created");
    assert_eq!(r.state.entity_type, "Project");
}

#[tokio::test]
async fn actor_project_full_lifecycle() {
    let system = ActorSystem::new("test-project-lifecycle");
    let actor = EntityActor::new("Project", "p-2", project_table_rw(), serde_json::json!({}));
    let actor_ref = system.spawn(actor, "p-2");

    // Created → Building
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "UpdateSpecs".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success, "UpdateSpecs should succeed: {:?}", r.error);
    assert_eq!(r.state.status, "Building");

    // Building → Verified
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Verify".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success, "Verify should succeed: {:?}", r.error);
    assert_eq!(r.state.status, "Verified");

    // Verified → Archived
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Archive".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success, "Archive should succeed: {:?}", r.error);
    assert_eq!(r.state.status, "Archived");
    assert_eq!(r.state.events.len(), 3);
}

#[tokio::test]
async fn actor_project_verify_requires_building_state() {
    let system = ActorSystem::new("test-project-guard");
    let actor = EntityActor::new("Project", "p-3", project_table_rw(), serde_json::json!({}));
    let actor_ref = system.spawn(actor, "p-3");

    // Created → cannot Verify directly
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Verify".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(!r.success, "Verify should fail from Created");
    assert_eq!(r.state.status, "Created");
}

// =========================================================================
// TENANT — Production EntityActor
// =========================================================================

#[tokio::test]
async fn actor_tenant_full_lifecycle() {
    let system = ActorSystem::new("test-tenant");
    let actor = EntityActor::new("Tenant", "t-1", tenant_table_rw(), serde_json::json!({}));
    let actor_ref = system.spawn(actor, "t-1");

    let r: EntityResponse = actor_ref.ask(EntityMsg::GetState, TIMEOUT).await.unwrap();
    assert_eq!(r.state.status, "Pending");

    // Deploy
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Deploy".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success, "Deploy: {:?}", r.error);
    assert_eq!(r.state.status, "Active");

    // Suspend
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Suspend".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Suspended");

    // Reactivate
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Reactivate".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Active");

    // Archive
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Archive".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Archived");
}

#[tokio::test]
async fn actor_tenant_cannot_deploy_archived() {
    let system = ActorSystem::new("test-tenant-guard");
    let actor = EntityActor::new("Tenant", "t-2", tenant_table_rw(), serde_json::json!({}));
    let actor_ref = system.spawn(actor, "t-2");

    // Pending → Active → Archived
    let _: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Deploy".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    let _: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Archive".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();

    // Archived → cannot Deploy
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Deploy".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(!r.success);
    assert_eq!(r.state.status, "Archived");
}

// =========================================================================
// CATALOG ENTRY — Production EntityActor
// =========================================================================

#[tokio::test]
async fn actor_catalog_publish_and_fork() {
    let system = ActorSystem::new("test-catalog");
    let actor = EntityActor::new(
        "CatalogEntry",
        "cat-1",
        catalog_table_rw(),
        serde_json::json!({}),
    );
    let actor_ref = system.spawn(actor, "cat-1");

    let r: EntityResponse = actor_ref.ask(EntityMsg::GetState, TIMEOUT).await.unwrap();
    assert_eq!(r.state.status, "Draft");

    // Publish
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Publish".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Published");

    // Fork (non-transitioning — stays Published)
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Fork".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Published");

    // Deprecate
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Deprecate".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Deprecated");
}

// =========================================================================
// COLLABORATOR — Production EntityActor
// =========================================================================

#[tokio::test]
async fn actor_collaborator_invite_accept_remove() {
    let system = ActorSystem::new("test-collaborator");
    let actor = EntityActor::new(
        "Collaborator",
        "col-1",
        collaborator_table_rw(),
        serde_json::json!({}),
    );
    let actor_ref = system.spawn(actor, "col-1");

    let r: EntityResponse = actor_ref.ask(EntityMsg::GetState, TIMEOUT).await.unwrap();
    assert_eq!(r.state.status, "Invited");

    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Accept".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Active");

    // ChangeRole — non-transitioning
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "ChangeRole".into(),
                params: serde_json::json!({"role": "editor"}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Active");

    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Remove".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Removed");
}

// =========================================================================
// VERSION — Production EntityActor
// =========================================================================

#[tokio::test]
async fn actor_version_lifecycle() {
    let system = ActorSystem::new("test-version");
    let actor = EntityActor::new("Version", "v-1", version_table_rw(), serde_json::json!({}));
    let actor_ref = system.spawn(actor, "v-1");

    let r: EntityResponse = actor_ref.ask(EntityMsg::GetState, TIMEOUT).await.unwrap();
    assert_eq!(r.state.status, "Created");

    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "MarkDeployed".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Deployed");

    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Supersede".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Superseded");
}

// =========================================================================
// MULTI-ACTOR — Production EntityActor independence
// =========================================================================

#[tokio::test]
async fn actor_multiple_system_entities_independent() {
    let system = ActorSystem::new("test-multi");

    let p = system.spawn(
        EntityActor::new(
            "Project",
            "proj-1",
            project_table_rw(),
            serde_json::json!({}),
        ),
        "proj-1",
    );
    let t = system.spawn(
        EntityActor::new(
            "Tenant",
            "tenant-1",
            tenant_table_rw(),
            serde_json::json!({}),
        ),
        "tenant-1",
    );
    let c = system.spawn(
        EntityActor::new(
            "CatalogEntry",
            "cat-1",
            catalog_table_rw(),
            serde_json::json!({}),
        ),
        "cat-1",
    );

    // Progress each independently
    let _: EntityResponse = p
        .ask(
            EntityMsg::Action {
                name: "UpdateSpecs".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    let _: EntityResponse = t
        .ask(
            EntityMsg::Action {
                name: "Deploy".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    let _: EntityResponse = c
        .ask(
            EntityMsg::Action {
                name: "Publish".into(),
                params: serde_json::json!({}),
            },
            TIMEOUT,
        )
        .await
        .unwrap();

    // Verify independent states
    let rp: EntityResponse = p.ask(EntityMsg::GetState, TIMEOUT).await.unwrap();
    let rt: EntityResponse = t.ask(EntityMsg::GetState, TIMEOUT).await.unwrap();
    let rc: EntityResponse = c.ask(EntityMsg::GetState, TIMEOUT).await.unwrap();

    assert_eq!(rp.state.status, "Building");
    assert_eq!(rt.state.status, "Active");
    assert_eq!(rc.state.status, "Published");
}

// =========================================================================
// CODEGEN — Tier 1 compiled types from system CSDL
// =========================================================================

#[test]
fn codegen_system_entities_produce_valid_modules() {
    use temper_codegen::generate_entity_module;
    use temper_spec::csdl::parse_csdl;
    use temper_spec::model::build_spec_model;

    let csdl_xml = SYSTEM_MODEL_CSDL_XML;
    let csdl = parse_csdl(csdl_xml).expect("system CSDL should parse");

    let spec = build_spec_model(csdl, std::collections::HashMap::new());

    // Generate Tier 1 compiled code for each system entity
    for entity_name in &[
        "Project",
        "Tenant",
        "CatalogEntry",
        "Collaborator",
        "Version",
    ] {
        let module = generate_entity_module(&spec, entity_name)
            .unwrap_or_else(|e| panic!("codegen for {entity_name} failed: {e}"));

        // Verify generated code contains expected structures
        assert!(
            module
                .source
                .contains(&format!("pub struct {}State", entity_name)),
            "{entity_name} should have a state struct:\n{}",
            &module.source[..200.min(module.source.len())]
        );
        assert!(
            module
                .source
                .contains(&format!("pub enum {}Msg", entity_name)),
            "{entity_name} should have a message enum"
        );
        assert!(
            module.source.contains("pub id:"),
            "{entity_name} should have an id field"
        );
        assert!(
            module.source.contains("pub status:"),
            "{entity_name} should have a status field"
        );
    }
}

#[test]
fn codegen_project_has_typed_fields() {
    use temper_codegen::generate_entity_module;
    use temper_spec::csdl::parse_csdl;
    use temper_spec::model::build_spec_model;

    let csdl_xml = SYSTEM_MODEL_CSDL_XML;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let spec = build_spec_model(csdl, std::collections::HashMap::new());

    let module = generate_entity_module(&spec, "Project").unwrap();

    // Project-specific fields from CSDL
    assert!(
        module.source.contains("pub name:"),
        "Project should have name field"
    );
    assert!(
        module.source.contains("pub description:"),
        "Project should have description field"
    );
    assert!(
        module.source.contains("Verify"),
        "Project should have Verify action"
    );
    assert!(
        module.source.contains("Archive"),
        "Project should have Archive action"
    );
    assert!(
        module.source.contains("UpdateSpecs"),
        "Project should have UpdateSpecs action"
    );
}

#[test]
fn codegen_tenant_has_project_reference() {
    use temper_codegen::generate_entity_module;
    use temper_spec::csdl::parse_csdl;
    use temper_spec::model::build_spec_model;

    let csdl_xml = SYSTEM_MODEL_CSDL_XML;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let spec = build_spec_model(csdl, std::collections::HashMap::new());

    let module = generate_entity_module(&spec, "Tenant").unwrap();

    assert!(
        module.source.contains("Deploy"),
        "Tenant should have Deploy action"
    );
    assert!(
        module.source.contains("Suspend"),
        "Tenant should have Suspend action"
    );
    assert!(
        module.source.contains("Reactivate"),
        "Tenant should have Reactivate action"
    );
}
