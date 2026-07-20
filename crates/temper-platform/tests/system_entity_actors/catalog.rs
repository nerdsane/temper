use super::*;

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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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

    // ChangeRole — non-transitioning
    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "ChangeRole".into(),
                params: serde_json::json!({"role": "editor"}),
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

    let r: EntityResponse = actor_ref
        .ask(
            EntityMsg::Action {
                name: "Remove".into(),
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
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
                cross_entity_booleans: std::collections::BTreeMap::new(),
                idempotency_key: None,
                state_timeout_precondition: None,
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Superseded");
}
