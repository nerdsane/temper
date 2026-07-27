//! Regression coverage for request-authority/resource namespace isolation.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::{ServerState, build_router};
use tower::ServiceExt;

fn test_state() -> ServerState {
    let (state, _) = common::build_default_state(17, "odata-authz-context-isolation");
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"
                permit(
                    principal is Agent,
                    action == Action::"create",
                    resource is Order
                ) when {
                    context.agentType == "trusted-worker" &&
                    context.agentTypeVerified == true &&
                    context.sessionId == "approved-session"
                };
            "#,
        )
        .expect("test policy must compile");
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-07-09T00:00:00Z".to_string(),
        }),
    );
    state
}

#[tokio::test]
async fn create_body_cannot_spoof_authenticated_context() {
    let response = build_router(test_state())
        .oneshot(
            Request::post("/tdata/Orders")
                .header("content-type", "application/json")
                .header("x-temper-principal-id", "attacker")
                .header("x-temper-principal-kind", "agent")
                .header("x-temper-agent-type", "untrusted-worker")
                .body(Body::from(
                    r#"{
                        "id": "order-spoof",
                        "agentType": "trusted-worker",
                        "agentTypeVerified": true,
                        "sessionId": "approved-session"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "OData entity fields must not satisfy identity or session conditions"
    );
}
