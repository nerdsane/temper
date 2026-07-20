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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(!r.success);
    assert_eq!(r.state.status, "Archived");
}

#[path = "system_entity_actors/catalog.rs"]
mod catalog;

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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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

#[path = "system_entity_actors/codegen.rs"]
mod codegen;
