//! ARN-389 / ADR-0172: operator bootstrap seeds `manage_policies`.
//!
//! Proves a virgin store's verified operator can manage Cedar, that
//! unverified / non-operator principals stay denied, that re-bootstrap is
//! idempotent, that OS-app Cedar still loads, and that the sole holder of a
//! new operator key retires the credential an earlier boot issued.

use std::collections::{BTreeMap, HashMap};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use temper_authz::{AuthzDecision, SecurityContext};
use temper_platform::bootstrap::{
    bootstrap_agent_specs, bootstrap_operator_credential, retire_other_operator_credentials,
};
use temper_platform::install_os_app;
use temper_platform::recovery::recover_cedar_policies;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::identity::{IdentityResolver, hash_token};
use temper_server::request_context::AgentContext;
use temper_server::state::PendingDecision;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

mod common;
use common::http::body_json;

const OPERATOR_KEY: &str = "tmpr_operator-bootstrap-cedar";
const DEVELOPER_KEY: &str = "tmpr_developer-bootstrap-cedar";
const POLICY_ID: &str = "operator-bootstrap-manage-policies";

fn virgin_state(tenant: &str) -> PlatformState {
    let state = PlatformState::new(None);
    bootstrap_agent_specs(&state, tenant, false, &BTreeMap::new());
    state
}

async fn virgin_state_with_store(tenant: &str) -> (PlatformState, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp policy db");
    let db_url = format!("file:{}", temp.path().join("policy.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create turso store");
    let mut state = virgin_state(tenant);
    state
        .server
        .set_storage_stack(StorageStack::from_turso(store));
    (state, temp)
}

fn manage_policies_attrs(tenant: &str) -> HashMap<String, serde_json::Value> {
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), json!(tenant));
    attrs.insert("tenant".to_string(), json!(tenant));
    attrs
}

fn authorize_manage_policies(
    state: &PlatformState,
    tenant: &str,
    ctx: &SecurityContext,
) -> AuthzDecision {
    state.server.authz.authorize_for_tenant(
        tenant,
        ctx,
        "manage_policies",
        "PolicySet",
        &manage_policies_attrs(tenant),
    )
}

fn unverified_operator_context() -> SecurityContext {
    let mut ctx = SecurityContext::from_resolved_identity("operator", "operator", None);
    ctx.principal
        .attributes
        .insert("agentTypeVerified".to_string(), json!(false));
    ctx.context_attrs
        .insert("agentTypeVerified".to_string(), json!(false));
    ctx
}

async fn issue_developer_credential(state: &PlatformState, tenant: &str, plaintext: &str) {
    let tenant_id = TenantId::new(tenant);
    let ctx = AgentContext::system();
    let key_hash = hash_token(plaintext);
    let _ = state
        .server
        .dispatch_tenant_action(
            &tenant_id,
            "AgentType",
            "developer-type",
            "Define",
            json!({
                "name": "developer",
                "system_prompt": "test",
                "tool_set": "local",
                "model": "none",
                "max_turns": "0",
                "adapter_config": "{}",
                "default_budget_cents": "0"
            }),
            &ctx,
        )
        .await;
    let _ = state
        .server
        .dispatch_tenant_action(
            &tenant_id,
            "AgentCredential",
            &key_hash,
            "Issue",
            json!({
                "agent_type_id": "developer-type",
                "agent_instance_id": "developer",
                "key_hash": key_hash,
                "key_prefix": plaintext.chars().take(8).collect::<String>(),
                "description": "non-operator test credential",
                "created_by": "test",
                "expires_at": ""
            }),
            &ctx,
        )
        .await;
}

fn statement_occurrences(haystack: &str, tenant: &str) -> usize {
    let needle = format!(r#"resource == PolicySet::"{tenant}""#);
    haystack.matches(&needle).count()
}

#[tokio::test]
async fn virgin_store_verified_operator_can_manage_policies() {
    let tenant = "acme";
    let state = virgin_state(tenant);
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");

    let operator = SecurityContext::from_resolved_identity("operator", "operator", None);
    let decision = authorize_manage_policies(&state, tenant, &operator);
    assert!(
        decision.is_allowed(),
        "verified operator must be allowed manage_policies on a virgin store, got {decision:?}"
    );

    let text = state
        .server
        .authz
        .get_tenant_policy_text(tenant)
        .expect("seeded policy text");
    assert!(text.contains(r#"Action::"manage_policies""#));
    assert!(text.contains(r#"PolicySet::"acme""#));
}

/// End-to-end guard against ever locking the authorization server (the real
/// operator) out of the god-mode identity entities. After bootstrap the
/// credential-resolved operator context must still be ALLOWED RegisterIssuer on
/// TrustedIssuer and BumpGeneration on PrincipalGeneration through the shared
/// verified-operator predicate — tightening it to require `agentTypeVerified`
/// must not touch the AS, which is always credential-verified.
#[tokio::test]
async fn bootstrapped_operator_can_manage_identity_entities() {
    let tenant = "acme";
    let state = virgin_state(tenant);
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");

    let operator = SecurityContext::from_resolved_identity("operator", "operator", None);
    let attrs = HashMap::new();

    for (action, entity) in [
        ("RegisterIssuer", "TrustedIssuer"),
        ("BumpGeneration", "PrincipalGeneration"),
    ] {
        let decision = state
            .server
            .authz
            .authorize_for_tenant(tenant, &operator, action, entity, &attrs);
        assert!(
            decision.is_allowed(),
            "the bootstrapped operator (the AS) must be allowed {action} on {entity}, got {decision:?}"
        );
    }

    // The self-declared, credential-unverified operator does NOT get the same
    // reach — the identity-entity gate now requires verification.
    let unverified = unverified_operator_context();
    for (action, entity) in [
        ("RegisterIssuer", "TrustedIssuer"),
        ("BumpGeneration", "PrincipalGeneration"),
    ] {
        let decision =
            state
                .server
                .authz
                .authorize_for_tenant(tenant, &unverified, action, entity, &attrs);
        assert!(
            !decision.is_allowed(),
            "an unverified operator-type context must not {action} on {entity}, got {decision:?}"
        );
    }
}

#[tokio::test]
async fn unverified_or_non_operator_cannot_manage_policies() {
    let tenant = "acme";
    let state = virgin_state(tenant);
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");

    let unverified = authorize_manage_policies(&state, tenant, &unverified_operator_context());
    assert!(
        !unverified.is_allowed(),
        "unverified operator must stay denied, got {unverified:?}"
    );

    let developer = SecurityContext::from_resolved_identity("developer", "developer", None);
    let denied = authorize_manage_policies(&state, tenant, &developer);
    assert!(
        !denied.is_allowed(),
        "non-operator must stay denied, got {denied:?}"
    );
}

#[tokio::test]
async fn rebootstrap_is_idempotent_and_persists_one_granular_row() {
    let tenant = "acme";
    let (state, _temp) = virgin_state_with_store(tenant).await;

    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");

    let text = state
        .server
        .authz
        .get_tenant_policy_text(tenant)
        .expect("seeded policy text");
    assert_eq!(
        statement_occurrences(&text, tenant),
        1,
        "re-bootstrap must not duplicate the live permit: {text}"
    );

    let rows = state
        .server
        .policy_store()
        .expect("policy store")
        .load_policies_for_tenant(tenant)
        .await
        .expect("load policies");
    let matches: Vec<_> = rows
        .iter()
        .filter(|row| row.policy_id == POLICY_ID)
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "re-bootstrap must not create duplicate granular rows: {rows:?}"
    );
    assert!(matches[0].enabled);
    assert!(
        matches[0]
            .cedar_text
            .contains(r#"Action::"manage_policies""#)
    );
}

#[tokio::test]
async fn existing_app_cedar_still_loads_after_operator_bootstrap() {
    let tenant = "app-tenant";
    let (state, _temp) = virgin_state_with_store(tenant).await;

    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");
    install_os_app(&state, tenant, "project-management")
        .await
        .expect("install project-management");

    let operator = SecurityContext::from_resolved_identity("operator", "operator", None);
    assert!(
        authorize_manage_policies(&state, tenant, &operator).is_allowed(),
        "operator manage_policies must survive app install"
    );

    let any = SecurityContext::from_resolved_identity("operator", "operator", None);
    let mut issue_attrs = HashMap::new();
    issue_attrs.insert("id".to_string(), json!("issue-1"));
    let create =
        state
            .server
            .authz
            .authorize_for_tenant(tenant, &any, "create", "Issue", &issue_attrs);
    assert!(
        create.is_allowed(),
        "project-management Issue.create must remain allowed after operator seed: {create:?}"
    );

    let rows = state
        .server
        .policy_store()
        .expect("policy store")
        .load_policies_for_tenant(tenant)
        .await
        .expect("load policies");
    assert!(
        rows.iter().any(|row| row.policy_id == POLICY_ID),
        "operator bootstrap row must remain: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| {
            row.policy_id == "project-management-issue"
                && row.cedar_text.contains("resource is Issue")
        }),
        "app Issue cedar row must persist: {rows:?}"
    );
}

#[tokio::test]
async fn recovered_granular_row_still_allows_verified_operator() {
    let tenant = "acme";
    let temp = tempfile::tempdir().expect("temp recovery db");
    let db_url = format!("file:{}", temp.path().join("policy.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create turso store");

    let mut seeded = virgin_state(tenant);
    seeded
        .server
        .set_storage_stack(StorageStack::from_turso(store.clone()));
    bootstrap_operator_credential(&seeded, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");

    let recovered = PlatformState::new(None);
    recover_cedar_policies(&recovered, &store).await;

    let operator = SecurityContext::from_resolved_identity("operator", "operator", None);
    let decision = authorize_manage_policies(&recovered, tenant, &operator);
    assert!(
        decision.is_allowed(),
        "recovered operator permit must allow manage_policies, got {decision:?}"
    );
}

/// The operator key reads its tenant's installs (`read_app_installs`), which
/// is how Katagami finds the folder an app is served from; a developer key
/// does not.
#[tokio::test]
async fn http_app_installs_read_allows_operator_and_denies_developer() {
    let tenant = "acme";
    let (state, _temp) = virgin_state_with_store(tenant).await;
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");
    issue_developer_credential(&state, tenant, DEVELOPER_KEY).await;
    let app = build_platform_router(state);

    for (key, expected) in [
        (OPERATOR_KEY, StatusCode::OK),
        (DEVELOPER_KEY, StatusCode::FORBIDDEN),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/genesis/apps/installs")
                    .header("Authorization", format!("Bearer {key}"))
                    .header("X-Tenant-Id", tenant)
                    .body(Body::empty())
                    .expect("GET installs"),
            )
            .await
            .expect("GET installs should run");
        assert_eq!(response.status(), expected, "{key}");
    }
}

#[tokio::test]
async fn http_policy_api_allows_operator_and_denies_developer() {
    let tenant = "acme";
    let (state, _temp) = virgin_state_with_store(tenant).await;
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");
    issue_developer_credential(&state, tenant, DEVELOPER_KEY).await;

    let app = build_platform_router(state);

    let allowed = app
        .clone()
        .oneshot(
            Request::get(format!("/api/tenants/{tenant}/policies"))
                .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
                .header("X-Tenant-Id", tenant)
                .body(Body::empty())
                .expect("operator GET policies"),
        )
        .await
        .expect("operator GET policies should run");
    assert_eq!(
        allowed.status(),
        StatusCode::OK,
        "verified operator GET /policies must succeed"
    );
    let body = body_json(allowed).await;
    let text = body["policy_text"].as_str().unwrap_or_default();
    assert!(text.contains(r#"Action::"manage_policies""#), "{body}");

    let listed = app
        .clone()
        .oneshot(
            Request::get(format!("/api/tenants/{tenant}/policies/list"))
                .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
                .header("X-Tenant-Id", tenant)
                .body(Body::empty())
                .expect("operator list policies"),
        )
        .await
        .expect("operator list should run");
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = body_json(listed).await;
    let empty_policies = Vec::new();
    let ids: Vec<&str> = listed_body["policies"]
        .as_array()
        .unwrap_or(&empty_policies)
        .iter()
        .filter_map(|row| row["policy_id"].as_str())
        .collect();
    assert!(
        ids.contains(&POLICY_ID),
        "list must include the bootstrap row: {listed_body}"
    );

    let denied = app
        .oneshot(
            Request::get(format!("/api/tenants/{tenant}/policies"))
                .header("Authorization", format!("Bearer {DEVELOPER_KEY}"))
                .header("X-Tenant-Id", tenant)
                .body(Body::empty())
                .expect("developer GET policies"),
        )
        .await
        .expect("developer GET policies should run");
    assert_eq!(
        denied.status(),
        StatusCode::FORBIDDEN,
        "non-operator GET /policies must be 403"
    );
}

#[tokio::test]
async fn denied_developer_cannot_self_approve_operator_can() {
    let tenant = "acme";
    let (state, _temp) = virgin_state_with_store(tenant).await;
    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");
    issue_developer_credential(&state, tenant, DEVELOPER_KEY).await;

    let developer_permit = format!(
        r#"permit(
  principal is Agent,
  action == Action::"manage_policies",
  resource == PolicySet::"{tenant}"
) when {{
  principal.agent_type == "developer" &&
  principal.agentTypeVerified == true
}};"#
    );
    let pending = PendingDecision::from_denial(
        tenant,
        "developer",
        "Assign",
        "Issue",
        "issue-1",
        json!({"id": "issue-1"}),
        "test denial",
        None,
    );
    let decision_id = pending.id.clone();
    state
        .server
        .persist_pending_decision(&pending)
        .await
        .expect("persist pending decision");

    let app = build_platform_router(state);
    let created = app
        .clone()
        .oneshot(
            Request::post(format!("/api/tenants/{tenant}/policies/create"))
                .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
                .header("X-Tenant-Id", tenant)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "policy_id": "developer-manage-policies",
                        "cedar_text": developer_permit,
                    })
                    .to_string(),
                ))
                .expect("create developer policy"),
        )
        .await
        .expect("create developer policy should run");
    assert_eq!(
        created.status(),
        StatusCode::CREATED,
        "operator must be able to add Cedar"
    );

    let approve_body = json!({
        "scope": {
            "principal": "this_agent",
            "action": "this_action",
            "resource": "this_resource",
            "duration": "always"
        }
    })
    .to_string();

    let self_approve = app
        .clone()
        .oneshot(
            Request::post(format!(
                "/api/tenants/{tenant}/decisions/{decision_id}/approve"
            ))
            .header("Authorization", format!("Bearer {DEVELOPER_KEY}"))
            .header("X-Tenant-Id", tenant)
            .header("content-type", "application/json")
            .body(Body::from(approve_body.clone()))
            .expect("developer self-approve"),
        )
        .await
        .expect("developer self-approve should run");
    assert_eq!(
        self_approve.status(),
        StatusCode::FORBIDDEN,
        "developer with manage_policies must still get 403 on their own decision"
    );

    let operator_approve = app
        .oneshot(
            Request::post(format!(
                "/api/tenants/{tenant}/decisions/{decision_id}/approve"
            ))
            .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
            .header("X-Tenant-Id", tenant)
            .header("content-type", "application/json")
            .body(Body::from(approve_body))
            .expect("operator approve"),
        )
        .await
        .expect("operator approve should run");
    assert_eq!(
        operator_approve.status(),
        StatusCode::OK,
        "verified operator must approve another agent's decision"
    );
    let approved = body_json(operator_approve).await;
    assert_eq!(approved["status"], "approved");
    assert!(
        approved["generated_policy"]
            .as_str()
            .unwrap_or_default()
            .contains(r#"Action::"Assign""#),
        "{approved}"
    );
}

/// A boot with a new operator key (a new key over restored data, or a
/// rotation) leaves the old key's credential live, as it always has: another
/// holder may still use it. A deployment that is the sole holder of its key
/// retires it (`retire_other_operator_credentials`, behind
/// `TEMPER_RETIRE_OLD_OPERATOR_KEYS`), so the old key loses its authority.
/// Credentials issued through the API stay, and the current key keeps working.
#[tokio::test]
async fn the_sole_holder_of_a_new_operator_key_retires_the_old_keys_credential() {
    const OLD_KEY: &str = "tmpr_operator-before-the-volume-was-lost";
    const NEW_KEY: &str = "tmpr_operator-made-at-the-next-start";
    let tenant = "default";
    let tenant_id = TenantId::new(tenant);
    let state = virgin_state(tenant);
    let resolver = IdentityResolver::new();
    let resolves = |key: &'static str| {
        let (resolver, state, tenant_id) = (&resolver, &state, &tenant_id);
        async move {
            resolver
                .resolve(&state.server, tenant_id, key)
                .await
                .is_ok()
        }
    };

    bootstrap_operator_credential(&state, OLD_KEY, tenant)
        .await
        .expect("first boot");
    issue_developer_credential(&state, tenant, DEVELOPER_KEY).await;
    bootstrap_operator_credential(&state, NEW_KEY, tenant)
        .await
        .expect("boot with a new key");
    assert!(
        resolves(OLD_KEY).await && resolves(NEW_KEY).await,
        "a boot alone keeps both: another holder may use the old key"
    );

    assert_eq!(
        retire_other_operator_credentials(&state, NEW_KEY, tenant).await,
        1
    );
    assert!(resolves(NEW_KEY).await, "the current key is the operator");
    assert!(!resolves(OLD_KEY).await, "the old key no longer works");
    let old = state
        .server
        .get_tenant_entity_state(&tenant_id, "AgentCredential", &hash_token(OLD_KEY))
        .await
        .expect("old credential");
    assert_eq!(old.state.status, "Revoked");
    assert!(
        resolves(DEVELOPER_KEY).await,
        "a credential issued through the API is not a boot's to retire"
    );
    assert_eq!(
        retire_other_operator_credentials(&state, NEW_KEY, tenant).await,
        0,
        "nothing more the next time"
    );
}

/// The same over a restart: a new server over the store an earlier one wrote
/// (an app's Temper restored from Turso with a new key) finds the old key's
/// credential in the store and retires it.
#[tokio::test]
async fn a_restart_with_a_new_operator_key_retires_the_old_keys_credential_from_the_store() {
    const OLD_KEY: &str = "tmpr_operator-written-to-the-store";
    const NEW_KEY: &str = "tmpr_operator-after-the-restart";
    let tenant = "default";
    let tenant_id = TenantId::new(tenant);
    let temp = tempfile::tempdir().expect("temp db");
    let db_url = format!("file:{}", temp.path().join("operator.db").display());
    let boot = |key: &'static str| {
        let db_url = db_url.clone();
        async move {
            let store = TursoEventStore::new(&db_url, None).await.expect("store");
            let mut state = virgin_state(tenant);
            state
                .server
                .set_storage_stack(StorageStack::from_turso(store));
            bootstrap_operator_credential(&state, key, tenant)
                .await
                .expect("boot");
            state
        }
    };
    drop(boot(OLD_KEY).await);
    let state = boot(NEW_KEY).await;
    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant_id, OLD_KEY)
            .await
            .is_ok(),
        "without retiring, the old key still works after the restart"
    );
    assert_eq!(
        retire_other_operator_credentials(&state, NEW_KEY, tenant).await,
        1
    );
    assert!(
        resolver
            .resolve(&state.server, &tenant_id, OLD_KEY)
            .await
            .is_err()
    );
    assert!(
        resolver
            .resolve(&state.server, &tenant_id, NEW_KEY)
            .await
            .is_ok()
    );
}

/// The catalog projects writes in the background: wait until it holds `n`.
async fn active_operator_ids(state: &PlatformState, tenant: &TenantId, n: usize) -> Vec<String> {
    for _ in 0..200 {
        let mut ids = state
            .server
            .active_credential_ids(tenant, "operator")
            .await
            .expect("the Turso store has a catalog");
        if ids.len() >= n {
            ids.sort();
            return ids;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the catalog never held {n} Active operator credentials");
}

#[tokio::test]
async fn a_changed_operator_key_leaves_the_old_one_active_and_counted() {
    let tenant = "acme";
    let (state, _temp) = virgin_state_with_store(tenant).await;
    let tenant_id = TenantId::new(tenant);
    const ROTATED_KEY: &str = "tmpr_operator-bootstrap-rotated";

    bootstrap_operator_credential(&state, OPERATOR_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");
    assert_eq!(
        active_operator_ids(&state, &tenant_id, 1).await,
        [hash_token(OPERATOR_KEY)]
    );

    bootstrap_operator_credential(&state, ROTATED_KEY, tenant)
        .await
        .expect("operator credential bootstrap should succeed");
    let mut both = vec![hash_token(OPERATOR_KEY), hash_token(ROTATED_KEY)];
    both.sort();
    assert_eq!(active_operator_ids(&state, &tenant_id, 2).await, both);
}
