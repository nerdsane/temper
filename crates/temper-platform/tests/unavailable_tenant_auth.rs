//! A tenant whose stored specs did not load at startup answers its own
//! credentialed requests with 503 `TenantUnavailable`, not 401.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::collections::BTreeMap;
use temper_platform::bootstrap::{bootstrap_agent_specs, bootstrap_system_tenant};
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::identity::hash_token;
use temper_server::request_context::AgentContext;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

mod common;
use common::http::body_json;

const TENANT: &str = "unavailable-auth-test";
const PLAINTEXT: &str = "tmpr_unavailable-tenant-saved-key";

async fn dispatch(
    state: &PlatformState,
    entity_type: &str,
    id: &str,
    action: &str,
    params: serde_json::Value,
) {
    let response = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            entity_type,
            id,
            action,
            params,
            &AgentContext::system(),
        )
        .await
        .unwrap_or_else(|e| panic!("dispatch {entity_type}.{action} failed: {e}"));
    assert!(
        response.success,
        "{entity_type}.{action}: {:?}",
        response.error
    );
}

/// Issue a credential for `TENANT` on a server over `store`.
async fn issue_saved_credential(store: &SimEventStore) {
    let mut state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    bootstrap_agent_specs(&state, TENANT, false, &BTreeMap::new());
    state
        .server
        .set_storage_stack(StorageStack::from_sim(store.clone(), None));
    dispatch(
        &state,
        "AgentType",
        "unavailable-type",
        "Define",
        serde_json::json!({
            "name": "unavailable-agent",
            "system_prompt": "test",
            "tool_set": "local",
            "model": "claude-sonnet-4-6",
            "max_turns": "200",
        }),
    )
    .await;
    let key_hash = hash_token(PLAINTEXT);
    dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Issue",
        serde_json::json!({
            "agent_type_id": "unavailable-type",
            "agent_instance_id": "unavailable-inst",
            "key_hash": key_hash,
            "key_prefix": "tmpr_test",
            "description": "saved credential for an unavailable tenant",
            "created_by": "test",
            "expires_at": ""
        }),
    )
    .await;
}

/// The same store after a restart in which the tenant's specs did not load:
/// startup left it out of the registry and marked it unavailable.
fn restarted_with_tenant_unavailable(store: &SimEventStore) -> PlatformState {
    let mut state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    state
        .server
        .set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
        .server
        .registry
        .write()
        .expect("registry writable")
        .mark_tenant_unavailable(TENANT, "stored constraints".to_string());
    state
}

#[tokio::test]
async fn a_saved_credential_for_an_unavailable_tenant_gets_503_tenant_unavailable() {
    let store = SimEventStore::no_faults(0);
    issue_saved_credential(&store).await;
    let state = restarted_with_tenant_unavailable(&store);

    let response = temper_platform::router::build_platform_router(state)
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {PLAINTEXT}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "TenantUnavailable", "body: {body}");
}

#[tokio::test]
async fn an_unknown_credential_for_an_available_tenant_still_gets_401() {
    let store = SimEventStore::no_faults(0);
    let state = restarted_with_tenant_unavailable(&store);

    let response = temper_platform::router::build_platform_router(state)
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", "Bearer tmpr_whatever")
                .header("X-Tenant-Id", "some-other-tenant")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
