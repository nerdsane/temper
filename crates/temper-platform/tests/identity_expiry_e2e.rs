//! Credential-expiry coverage for the Class A authentication edge.

use std::collections::BTreeMap;

use temper_platform::bootstrap::{bootstrap_agent_specs, bootstrap_system_tenant};
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::identity::{IdentityResolver, hash_token};
use temper_server::request_context::AgentContext;

const TENANT: &str = "identity-expiry-test";

async fn dispatch(
    state: &PlatformState,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) {
    let response = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::system(),
        )
        .await
        .unwrap_or_else(|error| panic!("dispatch {entity_type}.{action} failed: {error}"));
    assert!(response.success, "dispatch failed: {:?}", response.error);
}

async fn issue_credential(state: &PlatformState, token: &str, expires_at: &str) {
    let key_hash = hash_token(token);
    dispatch(
        state,
        "AgentCredential",
        &key_hash,
        "Issue",
        serde_json::json!({
            "agent_type_id": "expiry-worker-type",
            "agent_instance_id": format!("instance-{token}"),
            "key_hash": key_hash,
            "key_prefix": "expiry",
            "description": "expiry boundary test",
            "created_by": "test",
            "expires_at": expires_at,
        }),
    )
    .await;
}

#[tokio::test]
async fn resolver_denies_expired_and_malformed_credentials_but_accepts_future_expiry() {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    bootstrap_agent_specs(&state, TENANT, false, &BTreeMap::new());
    dispatch(
        &state,
        "AgentType",
        "expiry-worker-type",
        "Define",
        serde_json::json!({
            "name": "expiry-worker",
            "system_prompt": "test",
            "tool_set": "local",
            "model": "none",
            "max_turns": "1",
            "adapter_config": "{}",
            "default_budget_cents": "0",
        }),
    )
    .await;

    issue_credential(&state, "expired-token", "2000-01-01T00:00:00Z").await;
    issue_credential(&state, "malformed-token", "not-rfc3339").await;
    issue_credential(&state, "future-token", "2999-01-01T00:00:00Z").await;

    let resolver = IdentityResolver::new();
    let tenant = TenantId::new(TENANT);
    assert!(
        resolver
            .resolve(&state.server, &tenant, "expired-token")
            .await
            .is_none(),
        "expired credential must not resolve"
    );
    assert!(
        resolver
            .resolve(&state.server, &tenant, "malformed-token")
            .await
            .is_none(),
        "malformed expiry must fail closed"
    );
    let future = resolver
        .resolve(&state.server, &tenant, "future-token")
        .await
        .expect("future-dated credential should resolve");
    assert_eq!(future.agent_type_name, "expiry-worker");
}
