//! External bound actions must never serve as an implicit entity create.
mod common;
#[path = "bound_action_existence/races.rs"]
mod races;
#[path = "bound_action_existence/review_regressions.rs"]
mod review_regressions;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::{persistence::EventStore, tenant::TenantId};
use temper_server::{
    ServerState,
    registry::{EntityVerificationResult, VerificationStatus},
};
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

fn fixture(store: SimEventStore, name: &str) -> ServerState {
    let state = common::build_default_state_with_store(store, name);
    // The existing Order fixture is verified, not replaced by a permissive stub.
    let result = temper_verify::cascade::VerificationCascade::from_ioa(common::ORDER_IOA)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    assert!(result.all_passed);
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: result.all_passed,
            levels: vec![],
            verified_at: "2026-09-07T00:00:00Z".into(),
        }),
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state
}

async fn post(state: &ServerState, path: &str, body: Value) -> (StatusCode, Value) {
    post_tenant(state, "default", path, body).await
}

async fn post_tenant(
    state: &ServerState,
    tenant: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity("caller", "test", None),
        ));
    let response = temper_server::build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn authorized_missing_bound_action_does_not_create_a_ghost() {
    let store = SimEventStore::no_faults(529);
    let state = fixture(store.clone(), "missing-bound-action");
    let (status, body) = post(
        &state,
        "/tdata/Orders('absent')/Temper.AddItem",
        json!({"ProductId":"p", "Quantity":1}),
    )
    .await;
    let events = store.read_events("default:Order:absent", 0).await.unwrap();
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "body={body}, target events={events:?}"
    );
    assert_eq!(body["error"]["code"], "EntityNotFound");
    assert!(events.is_empty(), "rejection must not append Created");
    assert_eq!(state.active_actor_count(), 0);
    assert!(!state.entity_exists(&TenantId::default(), "Order", "absent"));
}

#[tokio::test]
async fn explicit_post_create_and_existing_cold_bound_action_work() {
    let store = SimEventStore::no_faults(529);
    let writer = fixture(store.clone(), "bound-create");
    let (status, body) = post(&writer, "/tdata/Orders", json!({"Id":"cold"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let reader = fixture(store.clone(), "bound-cold");
    assert_eq!(reader.active_actor_count(), 0);
    let (status, body) = post(
        &reader,
        "/tdata/Orders('cold')/Temper.AddItem",
        json!({"ProductId":"p", "Quantity":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = store.read_events("default:Order:cold", 0).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "Created");
    assert_eq!(events[1].event_type, "AddItem");
}

#[tokio::test]
async fn denied_missing_and_cold_targets_disclose_no_existence_and_spawn_nothing() {
    let store = SimEventStore::no_faults(529);
    let writer = fixture(store.clone(), "denied-setup");
    assert_eq!(
        post(&writer, "/tdata/Orders", json!({"Id":"cold"})).await.0,
        StatusCode::CREATED
    );
    let state = fixture(store.clone(), "denied-reader");
    state
        .authz
        .reload_tenant_policies("default", "forbid(principal, action, resource);")
        .unwrap();
    for id in ["cold", "absent"] {
        let (status, body) = post(
            &state,
            &format!("/tdata/Orders('{id}')/Temper.AddItem"),
            json!({"ProductId":"p", "Quantity":1}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"]["code"], "AuthorizationDenied");
        assert!(!state.entity_exists(&TenantId::default(), "Order", id));
    }
    assert_eq!(state.active_actor_count(), 0);
    assert_eq!(store.dump_journal("default:Order:cold").len(), 1);
    assert!(store.dump_journal("default:Order:absent").is_empty());
}

#[tokio::test]
async fn other_tenant_entity_is_not_existence_authority() {
    let store = SimEventStore::no_faults(529);
    let state = fixture(store.clone(), "tenant-separation");
    assert_eq!(
        post(&state, "/tdata/Orders", json!({"Id":"same"})).await.0,
        StatusCode::CREATED
    );
    state.registry.write().unwrap().register_tenant(
        "other",
        temper_spec::csdl::parse_csdl(common::CSDL_XML).unwrap(),
        common::CSDL_XML.into(),
        &[("Order", common::ORDER_IOA)],
    );
    state
        .authz
        .reload_tenant_policies("other", "permit(principal, action, resource);")
        .unwrap();
    let (status, body) = post_tenant(
        &state,
        "other",
        "/tdata/Orders('same')/Temper.AddItem",
        json!({"ProductId":"p", "Quantity":1}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "EntityNotFound");
    assert!(!state.entity_exists(&TenantId::new("other"), "Order", "same"));
    assert!(store.dump_journal("other:Order:same").is_empty());
    assert_eq!(store.dump_journal("default:Order:same").len(), 1);
}

#[tokio::test]
async fn tombstones_override_cold_and_stale_resident_targets() {
    let store = SimEventStore::no_faults(529);
    let writer = fixture(store.clone(), "delete-writer");
    assert_eq!(
        post(&writer, "/tdata/Orders", json!({"Id":"deleted"}))
            .await
            .0,
        StatusCode::CREATED
    );
    let stale = fixture(store.clone(), "stale-reader");
    stale
        .get_tenant_entity_state(&TenantId::default(), "Order", "deleted")
        .await
        .unwrap();
    writer
        .delete_tenant_entity(&TenantId::default(), "Order", "deleted")
        .await
        .unwrap();
    let cold = fixture(store.clone(), "deleted-cold");
    for state in [&stale, &cold] {
        let (status, body) =
            post(state, "/tdata/Orders('deleted')/Temper.AddItem", json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "EntityNotFound");
    }
    assert_eq!(cold.active_actor_count(), 0);
    assert!(!cold.entity_exists(&TenantId::default(), "Order", "deleted"));
    assert_eq!(store.dump_journal("default:Order:deleted").len(), 2);
}

#[tokio::test]
async fn unreadable_authority_is_not_not_found_even_with_a_resident_actor() {
    for present in [false, true] {
        let store = SimEventStore::no_faults(529);
        let state = fixture(store.clone(), "unavailable-reader");
        if present {
            assert_eq!(
                post(&state, "/tdata/Orders", json!({"Id":"target"}))
                    .await
                    .0,
                StatusCode::CREATED
            );
        }
        let before = store.dump_journal("default:Order:target");
        store.fail_next_reads("default:Order:target", 1);
        let (status, body) =
            post(&state, "/tdata/Orders('target')/Temper.AddItem", json!({})).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert_eq!(body["error"]["code"], "ReadError");
        assert_eq!(
            serde_json::to_value(store.dump_journal("default:Order:target")).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        assert_eq!(state.active_actor_count(), u64::from(present));
        assert_eq!(
            state.entity_exists(&TenantId::default(), "Order", "target"),
            present
        );
    }
}

#[tokio::test]
async fn corrupt_snapshot_fails_closed_without_bootstrap() {
    let store = SimEventStore::no_faults(529);
    store
        .save_snapshot("default:Order:corrupt", 1, b"invalid snapshot")
        .await
        .unwrap();
    let state = fixture(store.clone(), "corrupt-reader");
    let (status, body) = post(&state, "/tdata/Orders('corrupt')/Temper.AddItem", json!({})).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(state.active_actor_count(), 0);
    assert!(!state.entity_exists(&TenantId::default(), "Order", "corrupt"));
    assert!(store.dump_journal("default:Order:corrupt").is_empty());
}

#[tokio::test]
async fn snapshot_backed_cold_target_and_trusted_internal_spawn_remain_supported() {
    let store = SimEventStore::no_faults(529);
    let writer = fixture(store.clone(), "internal-spawn");
    let response = common::dispatch(
        &writer,
        &TenantId::default(),
        "Order",
        "internal",
        "AddItem",
        json!({"ProductId":"p", "Quantity":1}),
    )
    .await
    .unwrap();
    assert!(response.success);
    assert_eq!(store.dump_journal("default:Order:internal").len(), 2);
    store
        .save_snapshot(
            "default:Order:internal",
            response.state.sequence_nr,
            &serde_json::to_vec(&response.state).unwrap(),
        )
        .await
        .unwrap();
    let cold = fixture(store.clone(), "snapshot-reader");
    let (status, body) = post(
        &cold,
        "/tdata/Orders('internal')/Temper.AddItem",
        json!({"ProductId":"p", "Quantity":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(store.dump_journal("default:Order:internal").len(), 3);
}

#[tokio::test]
async fn corrupt_journal_and_stale_catalog_cannot_authorize_bootstrap() {
    let store = SimEventStore::no_faults(529);
    let state = fixture(store.clone(), "corrupt-journal");
    assert_eq!(
        post(&state, "/tdata/Orders", json!({"Id":"corrupt"}))
            .await
            .0,
        StatusCode::CREATED
    );
    let mut corrupt = store.dump_journal("default:Order:corrupt")[0].clone();
    corrupt.payload = json!({"unreadable":"event"});
    corrupt.event_type = "AddItem".into();
    store
        .append("default:Order:corrupt", 1, &[corrupt])
        .await
        .unwrap();
    let cold = fixture(store.clone(), "corrupt-journal-reader");
    let (status, body) = post(&cold, "/tdata/Orders('corrupt')/Temper.AddItem", json!({})).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "ReadError");
    assert_eq!(cold.active_actor_count(), 0);
    assert!(!cold.entity_exists(&TenantId::default(), "Order", "corrupt"));
    assert_eq!(store.dump_journal("default:Order:corrupt").len(), 2);

    cold.entity_index
        .write()
        .unwrap()
        .entry("default:Order".into())
        .or_default()
        .insert("stale".into());
    let (status, body) = post(&cold, "/tdata/Orders('stale')/Temper.AddItem", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(cold.active_actor_count(), 0);
    assert!(store.dump_journal("default:Order:stale").is_empty());
}
