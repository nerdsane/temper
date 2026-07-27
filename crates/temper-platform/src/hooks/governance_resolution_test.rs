use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{ActionScope, DurationScope, PolicyScopeMatrix, PrincipalScope, ResourceScope};
use temper_runtime::TenantId;
use temper_server::authz::DecisionPolicyReceipt;
use temper_server::request_context::AgentContext;
use tower::ServiceExt;

use crate::bootstrap::{SYSTEM_TENANT, bootstrap_system_tenant};
use crate::state::PlatformState;

fn platform_state() -> PlatformState {
    let mut state = PlatformState::new(None);
    state.server.secrets_vault = Some(std::sync::Arc::new(
        temper_server::secrets::vault::SecretsVault::new(&[7; 32]),
    ));
    state
}

async fn create_decision(state: &PlatformState, id: &str) {
    state
        .server
        .dispatch_tenant_action(
            &TenantId::new(SYSTEM_TENANT),
            "GovernanceDecision",
            id,
            "CreateGovernanceDecision",
            serde_json::json!({
                "tenant": "default",
                "agent_id": "agent-1",
                "action_name": "read",
                "resource_type": "Order",
                "resource_id": "order-1",
                "denial_reason": "denied",
                "scope": "narrow",
                "pending_decision_id": format!("pd-{id}"),
            }),
            &AgentContext::for_service("platform-dispatch"),
        )
        .await
        .expect("create GovernanceDecision");
}

#[tokio::test]
async fn denial_becomes_terminal_only_after_effect_chain_finishes() {
    let state = platform_state();
    bootstrap_system_tenant(&state, &BTreeMap::new());
    let id = "gd-finalize-denial";
    create_decision(&state, id).await;

    let response = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(SYSTEM_TENANT),
            "GovernanceDecision",
            id,
            "Deny",
            serde_json::json!({
                "decided_by": "reviewer",
                "denial_reason": "denied by reviewer",
            }),
            &AgentContext::for_service("platform-dispatch"),
        )
        .await
        .expect("deny GovernanceDecision");
    assert!(response.success);
    assert_eq!(response.state.status, "Denying");
    let terminal = state
        .server
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", id)
        .await
        .expect("read finalized GovernanceDecision");
    assert_eq!(terminal.state.status, "Denied");
}

#[tokio::test]
async fn failed_callback_keeps_decision_nonterminal() {
    let state = platform_state();
    bootstrap_system_tenant(&state, &BTreeMap::new());
    let id = "gd-denial-callback-failure";
    create_decision(&state, id).await;
    let capability = state
        .server
        .mint_governance_callback_capability(
            id,
            SYSTEM_TENANT,
            "GovernanceDecision",
            "missing-governance-target",
            "FinalizeApproval",
            "FinalizeDenial",
        )
        .expect("mint callback capability");
    state
        .server
        .dispatch_tenant_action(
            &TenantId::new(SYSTEM_TENANT),
            "GovernanceDecision",
            id,
            "RegisterCallback",
            serde_json::json!({
                "callback_tenant": SYSTEM_TENANT,
                "callback_entity_set": "GovernanceDecisions",
                "callback_entity_id": "missing-governance-target",
                "callback_on_approve": "FinalizeApproval",
                "callback_on_deny": "FinalizeDenial",
                "callback_capability": capability,
            }),
            &AgentContext::for_service("governance-service"),
        )
        .await
        .expect("register callback");

    let mut context = AgentContext::for_service("platform-dispatch");
    context.idempotency_key = Some("deny-with-failed-callback".to_string());
    let response = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(SYSTEM_TENANT),
            "GovernanceDecision",
            id,
            "Deny",
            serde_json::json!({
                "decided_by": "reviewer",
                "denial_reason": "denied by reviewer",
            }),
            &context,
        )
        .await
        .expect("return failed effect result");
    assert!(!response.success);
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|error| error.contains("DispatchCallback"))
    );
    let progress = state
        .server
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", id)
        .await
        .expect("read nonterminal GovernanceDecision");
    assert_eq!(progress.state.status, "Denying");
}

#[tokio::test]
async fn failed_policy_receipt_keeps_approval_nonterminal() {
    let state = platform_state();
    bootstrap_system_tenant(&state, &BTreeMap::new());
    let id = "gd-approval-receipt-failure";
    create_decision(&state, id).await;
    let mut context = AgentContext::for_service("platform-dispatch");
    context.idempotency_key = Some("approve-with-invalid-receipt".to_string());

    let response = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(SYSTEM_TENANT),
            "GovernanceDecision",
            id,
            "Approve",
            serde_json::json!({
                "decided_by": "reviewer",
                "scope": "not-a-policy-receipt",
                "generated_policy": "permit(principal, action, resource);",
            }),
            &context,
        )
        .await
        .expect("return failed effect result");
    assert!(!response.success);
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid decision policy receipt")),
        "unexpected approval failure: {response:?}"
    );
    let progress = state
        .server
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", id)
        .await
        .expect("read nonterminal GovernanceDecision");
    assert_eq!(progress.state.status, "Approving");
}

#[tokio::test]
async fn direct_approval_with_valid_actor_receipt_but_no_durable_owner_fails_closed() {
    let state = platform_state();
    bootstrap_system_tenant(&state, &BTreeMap::new());
    let id = "gd-direct-receipt-bypass";
    let pending_id = format!("pd-{id}");
    create_decision(&state, id).await;
    let matrix = PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::ThisAction,
        resource: ResourceScope::ThisResource,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };
    let policy = temper_authz::generate_cedar_from_matrix(
        "agent-1", "Agent", "read", "Order", "order-1", &matrix,
    )
    .expect("generate exact actor receipt policy");
    state
        .server
        .authz
        .reload_tenant_policies_named(
            "default",
            &[(format!("decision:{pending_id}"), policy.clone())],
        )
        .expect("activate process-local forged prerequisite");
    let receipt = DecisionPolicyReceipt {
        pending_decision_id: pending_id,
        governance_decision_id: id.to_string(),
        principal_kind: "Agent".to_string(),
        scope_matrix: matrix,
    };
    let response = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(SYSTEM_TENANT),
            "GovernanceDecision",
            id,
            "Approve",
            serde_json::json!({
                "decided_by": "attacker",
                "scope": receipt.encode().expect("encode receipt"),
                "generated_policy": policy,
            }),
            &AgentContext::for_service("platform-dispatch"),
        )
        .await
        .expect("return failed durable verification result");
    assert!(!response.success);
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|error| error.contains("durable metadata is not configured")),
        "direct actor-only receipt unexpectedly passed: {response:?}"
    );
    let progress = state
        .server
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", id)
        .await
        .expect("read failed direct approval");
    assert_eq!(progress.state.status, "Approving");
}

#[tokio::test]
async fn noncanonical_service_cannot_start_governance_resolution() {
    let state = platform_state();
    bootstrap_system_tenant(&state, &BTreeMap::new());
    let id = "gd-direct-service-bypass";
    create_decision(&state, id).await;
    let before = state
        .server
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", id)
        .await
        .expect("read decision before rejected service dispatch");
    let error = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(SYSTEM_TENANT),
            "GovernanceDecision",
            id,
            "Approve",
            serde_json::json!({
                "decided_by": "attacker",
                "scope": "forged",
                "generated_policy": "permit(principal, action, resource);",
            }),
            &AgentContext::for_service("governance-service"),
        )
        .await
        .expect_err("generic internal service must not enter Approving");
    assert!(error.contains("canonical durable decision API"));
    let unchanged = state
        .server
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", id)
        .await
        .expect("read unchanged decision");
    assert_eq!(unchanged.state.sequence_nr, before.state.sequence_nr);
    assert_eq!(unchanged.state.status, "Pending");
}

#[tokio::test]
async fn odata_registration_requires_exact_capability_header_and_body() {
    let state = platform_state();
    bootstrap_system_tenant(&state, &BTreeMap::new());
    let id = "gd-odata-capability";
    create_decision(&state, id).await;
    let capability = state
        .server
        .mint_governance_callback_capability(
            id,
            SYSTEM_TENANT,
            "GovernanceDecision",
            id,
            "FinalizeApproval",
            "FinalizeDenial",
        )
        .expect("mint callback capability");
    let body = serde_json::json!({
        "callback_tenant": SYSTEM_TENANT,
        "callback_entity_set": "GovernanceDecisions",
        "callback_entity_id": id,
        "callback_on_approve": "FinalizeApproval",
        "callback_on_deny": "FinalizeDenial",
    });
    let uri = format!("/tdata/GovernanceDecisions('{id}')/temper-system.RegisterCallback");
    let request = |body: &serde_json::Value, capability: Option<&str>| {
        let mut builder = Request::post(&uri)
            .header("content-type", "application/json")
            .header("x-tenant-id", SYSTEM_TENANT)
            .header("x-temper-principal-kind", "admin");
        if let Some(capability) = capability {
            builder = builder.header("x-temper-callback-capability", capability);
        }
        builder
            .body(Body::from(body.to_string()))
            .expect("build callback request")
    };

    let missing = crate::router::build_platform_router(state.clone())
        .oneshot(request(&body, None))
        .await
        .expect("missing-capability response");
    assert_eq!(missing.status(), StatusCode::FORBIDDEN);

    let mut tampered = body.clone();
    tampered["callback_on_approve"] = serde_json::json!("DeleteEverything");
    let tampered = crate::router::build_platform_router(state.clone())
        .oneshot(request(&tampered, Some(&capability)))
        .await
        .expect("tampered-capability response");
    assert_eq!(tampered.status(), StatusCode::FORBIDDEN);

    let accepted = crate::router::build_platform_router(state.clone())
        .oneshot(request(&body, Some(&capability)))
        .await
        .expect("accepted-capability response");
    assert_eq!(accepted.status(), StatusCode::OK);
    let registered = state
        .server
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", id)
        .await
        .expect("read registered capability");
    assert_eq!(
        registered.state.fields["callback_capability"],
        serde_json::Value::String(capability)
    );
}

#[tokio::test]
async fn direct_invalid_registration_is_rejected_before_actor_commit() {
    let state = platform_state();
    bootstrap_system_tenant(&state, &BTreeMap::new());
    let id = "gd-direct-invalid-capability";
    create_decision(&state, id).await;
    let before = state
        .server
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", id)
        .await
        .expect("read decision before rejected registration");

    let error = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(SYSTEM_TENANT),
            "GovernanceDecision",
            id,
            "RegisterCallback",
            serde_json::json!({
                "callback_tenant": SYSTEM_TENANT,
                "callback_entity_set": "GovernanceDecisions",
                "callback_entity_id": "victim",
                "callback_on_approve": "FinalizeApproval",
                "callback_on_deny": "FinalizeDenial",
            }),
            &AgentContext::for_service("governance-service"),
        )
        .await
        .expect_err("unsigned direct registration must fail");
    assert!(error.contains("target-minted capability"));
    let unchanged = state
        .server
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", id)
        .await
        .expect("read unchanged decision");
    assert_eq!(unchanged.state.sequence_nr, before.state.sequence_nr);
    assert_eq!(
        unchanged.state.fields.get("callback_entity_id"),
        before.state.fields.get("callback_entity_id")
    );
}
