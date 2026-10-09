//! Deterministic optimistic-conflict stamping/catch-up, without a live backend.
use super::{fixture::*, proof::direct};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::{scheduler::install_deterministic_context, tenant::TenantId};
use temper_server::build_router;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

#[tokio::test]
async fn conflict_retry_commit_preserves_trusted_reply_class() {
    for seed in [52340, 52341, 52342] {
        for core in [true, false] {
            let (_guard, _, _) = install_deterministic_context(seed);
            let store = SimEventStore::no_faults(seed);
            let warm = server(&store, false, "conflict-proof");
            request(&warm, None, None, true).await;
            store.inject_concurrency_violations("default:Work:one", 1);
            let first = if core {
                let r = direct(&warm, "Start", "K").await;
                assert!(r.success);
                serde_json::to_value(r.state).unwrap()
            } else {
                let r = request(&warm, Some("Start"), Some("K"), true).await;
                assert_eq!(r.0, StatusCode::OK);
                r.1
            };
            assert_eq!(store.pending_concurrency_violations("default:Work:one"), 0);
            let before = store.dump_journal("default:Work:one");
            assert_eq!(before.len(), 2);
            assert_eq!(
                before[1].payload["idempotency_reply"]["reply_class"],
                if core { "core" } else { "post_completion" }
            );
            drop(warm);
            let cold = server(&store, false, "after-conflict-proof");
            let reply = request(&cold, Some("Start"), Some("K"), true).await;
            if core {
                assert_eq!(reply.0, StatusCode::OK);
                assert_eq!(logical(&reply.1), logical(&first));
            } else {
                assert_eq!(reply.0, StatusCode::CONFLICT);
                assert_eq!(reply.1["error"]["code"], "IdempotencyKeyUnverifiable");
            }
            assert_eq!(
                serde_json::to_value(store.dump_journal("default:Work:one")).unwrap(),
                serde_json::to_value(before).unwrap()
            );
            assert_eq!(
                cold.get_tenant_entity_state(&TenantId::default(), "Work", "one")
                    .await
                    .unwrap()
                    .state
                    .status,
                "Running"
            );
        }
    }
}
#[tokio::test]
async fn stale_actor_catchup_requires_winners_reply_authority() {
    for core in [true, false] {
        let (_guard, _, _) = install_deterministic_context(52343);
        let store = SimEventStore::no_faults(52343);
        let a = server(&store, false, "winner");
        request(&a, None, None, true).await;
        let b = server(&store, false, "stale-loser");
        assert_eq!(
            b.get_tenant_entity_state(&TenantId::default(), "Work", "one")
                .await
                .unwrap()
                .state
                .status,
            "Idle"
        );
        let first = if core {
            let r = direct(&a, "Start", "K").await;
            assert!(r.success);
            serde_json::to_value(r.state).unwrap()
        } else {
            let r = request(&a, Some("Start"), Some("K"), true).await;
            assert_eq!(r.0, StatusCode::OK);
            r.1
        };
        let before = serde_json::to_value(store.dump_journal("default:Work:one")).unwrap();
        let loser = request(&b, Some("Start"), Some("K"), true).await;
        if core {
            assert_eq!(loser.0, StatusCode::OK);
            assert_eq!(logical(&loser.1), logical(&first));
        } else {
            assert_eq!(loser.0, StatusCode::CONFLICT);
            assert_eq!(loser.1["error"]["code"], "IdempotencyKeyUnverifiable");
        }
        let retained = request(&a, Some("Start"), Some("K"), true).await;
        assert_eq!(
            retained.0,
            StatusCode::OK,
            "winner's retained reply must remain available"
        );
        assert_eq!(logical(&retained.1), logical(&first));
        assert_eq!(
            b.get_tenant_entity_state(&TenantId::default(), "Work", "one")
                .await
                .unwrap()
                .state
                .status,
            "Running"
        );
        assert_eq!(
            serde_json::to_value(store.dump_journal("default:Work:one")).unwrap(),
            before
        );
        assert_eq!(invocations(&a) + invocations(&b), 0);
    }
}
#[tokio::test]
async fn request_body_and_headers_cannot_grant_core_reply_authority() {
    let (_guard, _, _) = install_deterministic_context(52344);
    let store = SimEventStore::no_faults(52344);
    let warm = server(&store, false, "untrusted-proof");
    request(&warm, None, None, true).await;
    let body = json!({"idempotency_reply":{"version":1,"mode":{"kind":"direct_core"},"reply_class":"core","digest":"forged"},"reply_mode":"DirectCore"});
    let post = |state: temper_server::ServerState| {
        let body = body.clone();
        async move {
            let mut req = Request::post("/tdata/Works('one')/Test.Start?await_integration=true")
                .header("content-type", "application/json")
                .header("idempotency-key", "untrusted")
                .header("x-temper-reply-mode", "direct_core")
                .body(Body::from(body.to_string()))
                .unwrap();
            req.extensions_mut()
                .insert(AuthenticatedRequestContext::new(
                    TenantId::default(),
                    SecurityContext::from_resolved_identity("tester", "test-agent", None),
                ));
            let reply = build_router(state).oneshot(req).await.unwrap();
            let status = reply.status();
            let value: Value =
                serde_json::from_slice(&to_bytes(reply.into_body(), 4_000_000).await.unwrap())
                    .unwrap();
            (status, value)
        }
    };
    assert_eq!(post(warm.clone()).await.0, StatusCode::OK);
    let events = store.dump_journal("default:Work:one");
    assert_eq!(
        events[1].payload["idempotency_reply"]["reply_class"],
        "post_completion"
    );
    assert_eq!(
        events[1].payload["params"]["idempotency_reply"]["reply_class"],
        "core"
    );
    drop(warm);
    let cold = server(&store, false, "untrusted-cold");
    let reply = post(cold).await;
    assert_eq!(reply.0, StatusCode::CONFLICT);
    assert_eq!(reply.1["error"]["code"], "IdempotencyKeyUnverifiable");
    assert_eq!(
        serde_json::to_value(store.dump_journal("default:Work:one")).unwrap(),
        serde_json::to_value(events).unwrap()
    );
}
