use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_server::{
    build_router,
    registry::{EntityVerificationResult, VerificationStatus},
};
use tower::ServiceExt;

async fn post(
    state: &ServerState,
    principal: &str,
    params: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::post("/tdata/Tasks('task')/Test.Start?await_integration=true")
        .header("content-type", "application/json")
        .header("idempotency-key", "same-key")
        .body(Body::from(params.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity(principal, "test-agent", None),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4_000_000).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(start_paused = true)]
async fn http_idempotency_waiter_cannot_apply_failed_owner_and_checks_auth_and_binding() {
    let (_guard, _, _) = install_deterministic_context(51912);
    let (state, _, adapter) = fixture(51912);
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "TimedTask",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-02-08T00:00:00Z".into(),
        }),
    );
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"permit(principal, action, resource) when { principal.id == "tester" };"#,
        )
        .unwrap();
    state
        .get_or_create_tenant_entity(
            &TenantId::default(),
            "TimedTask",
            "task",
            json!({"Id":"task"}),
        )
        .await
        .unwrap();
    adapter.fail.store(true, Ordering::SeqCst);
    let mut owner = Box::pin(post(&state, "tester", json!({})));
    tokio::select! { biased;
        response = &mut owner => panic!("owner returned before adapter: {response:?}"),
        _ = adapter.started.notified() => {}
    }
    assert_eq!(
        post(&state, "intruder", json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&state, "tester", json!({"different":true})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut retry = Box::pin(post(&state, "tester", json!({})));
    for _ in 0..32 {
        assert!(
            poll!(retry.as_mut()).is_pending(),
            "HTTP duplicate returned before owner effects"
        );
        tokio::task::yield_now().await;
    }
    adapter.finish.notify_one();
    let (owner, retry) = tokio::join!(owner, retry);
    assert!(!owner.0.is_success(), "{owner:?}");
    assert_eq!(
        retry, owner,
        "all overlapping callers must observe the owner's failure"
    );
    let binding = temper_server::idempotency::request_binding("Start", &json!({}));
    assert!(matches!(
        state.idempotency_cache.lookup_after_completion(
            "default:TimedTask:task",
            "same-key",
            &binding
        ),
        temper_server::idempotency::IdempotencyLookup::Miss
    ));
    adapter.fail.store(false, Ordering::SeqCst);
    adapter.finish.notify_one();
    let recovered = post(&state, "tester", json!({})).await;
    assert_eq!(recovered.0, StatusCode::OK, "{recovered:?}");
    assert_eq!(post(&state, "tester", json!({})).await, recovered);
    assert_eq!(
        post(&state, "intruder", json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
}
