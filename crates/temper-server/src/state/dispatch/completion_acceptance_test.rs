//! Route composition, real worker phase recovery, and tenant/capacity acceptance.
use super::tests::fixture::*;
use crate::registry::{EntityVerificationResult, VerificationStatus};
use crate::{ServerState, request_context::AgentContext, state::DispatchExtOptions};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use futures_util::poll;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::{scheduler::install_deterministic_context, tenant::TenantId};
use temper_spec::csdl::parse_csdl;
use tokio::sync::{Notify, Semaphore};
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Source"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Test.Source"/></Action><EntityContainer Name="Container"><EntitySet Name="Sources" EntityType="Test.Source"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

async fn post(
    state: &ServerState,
    principal: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::post("/tdata/Sources('one')/Test.Start")
        .header("content-type", "application/json")
        .header("idempotency-key", "K")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity(principal, "test-agent", None),
        ));
    let response = crate::build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4_000_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn pending<F: std::future::Future>(future: &mut std::pin::Pin<Box<F>>) {
    for _ in 0..32 {
        assert!(
            poll!(future.as_mut()).is_pending(),
            "requested terminal milestone returned early"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn http_duplicate_after_effects_ready_joins_reactions_and_preserves_authorization() {
    let (_guard, _, _) = install_deterministic_context(51996);
    let (state, store, barrier) = reaction_fixture(false).await;
    state
        .registry
        .write()
        .unwrap()
        .try_register_tenant_with_constraints(
            "default",
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[],
            None,
            true,
        )
        .unwrap();
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Source",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "test".into(),
        }),
    );
    state
        .authz
        .reload_tenant_policies(
            "default",
            "permit(principal, action, resource) when { principal.id == \"tester\" };",
        )
        .unwrap();
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Source", "one", json!({"Id":"one"}))
        .await
        .unwrap();
    let mut first = Box::pin(post(&state, "tester", json!({})));
    tokio::select! { biased;
        reply = &mut first => panic!("HTTP returned before dependent commit: {reply:?}"),
        _ = barrier.entered.notified() => {}
    }
    // Entering this reaction proves source effects are ready, not terminal.
    assert_eq!(
        post(&state, "intruder", json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&state, "tester", json!({"different":true})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut retry = Box::pin(post(&state, "tester", json!({})));
    pending(&mut retry).await;
    barrier.release.add_permits(1);
    let (first, retry) = tokio::join!(first, retry);
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first, retry);
    assert_eq!(post(&state, "tester", json!({})).await, first);
    for kind in ["Source", "Child", "Leaf"] {
        assert_eq!(action_count(&store, kind, "Start"), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn injected_real_worker_handoff_recovers_only_unstarted_reactions() {
    let (_guard, _, _) = install_deterministic_context(51997);
    let (state, store, adapter) = credential_fixture().await;
    let work = state
        .registry
        .read()
        .unwrap()
        .get_spec(&TenantId::default(), "Work")
        .unwrap()
        .ioa_source
        .clone()
        + "\n[[action.triggers]]\nname=\"child\"\nkind=\"entity\"\ntarget_entity=\"Child\"\ntarget_action=\"Start\"\nresolve_target={kind=\"same_id\"}\n";
    let child = reaction_spec("Child", &[]);
    state
        .registry
        .write()
        .unwrap()
        .try_register_tenant_with_constraints(
            "default",
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Work", &work), ("Child", &child)],
            None,
            true,
        )
        .unwrap();
    state.rebuild_reaction_dispatcher();
    *state
        .idempotency_cache
        .reaction_handoff_once
        .lock()
        .unwrap() = Some(("default:Work:one".into(), "K".into()));
    let mut first = Box::pin(run(&state, "Work", true, true));
    tokio::select! { biased;
        reply = &mut first => panic!("effects returned before adapter: {reply:?}"),
        _ = adapter.started.notified() => {}
    }
    adapter.finish.add_permits(1);
    let first = first.await;
    assert!(!first.success);
    assert!(first.error.unwrap().contains("before reactions started"));
    assert_eq!(action_count(&store, "Child", "Start"), 0);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        state
            .idempotency_cache
            .continuation_slots
            .available_permits(),
        63
    );
    let recovered = tokio::time::timeout(Duration::from_secs(1), run(&state, "Work", false, true))
        .await
        .expect("recovery reran completed effects");
    assert!(recovered.success);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(action_count(&store, "Work", "Start"), 1);
    assert_eq!(action_count(&store, "Child", "Start"), 1);
    let hash = adapter.hashes.lock().unwrap()[0].clone();
    assert_eq!(credential_status(&state, &hash).await, "Revoked");
    assert_eq!(
        action_count_id(&store, "AgentCredential", &hash, "Revoke"),
        1
    );
    assert_eq!(
        state
            .idempotency_cache
            .continuation_slots
            .available_permits(),
        64
    );
    assert_eq!(
        serde_json::to_value(run(&state, "Work", true, true).await).unwrap(),
        serde_json::to_value(recovered).unwrap()
    );
}

struct Gate {
    started: Notify,
    finish: Semaphore,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl crate::adapters::AgentAdapter for Gate {
    fn adapter_type(&self) -> &str {
        "tenant-gate"
    }
    async fn execute(
        &self,
        _: crate::adapters::AdapterContext,
    ) -> Result<crate::adapters::AdapterResult, crate::adapters::AdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.finish.acquire().await.unwrap().forget();
        Ok(crate::adapters::AdapterResult::success(json!({}), 0))
    }
}

#[tokio::test(start_paused = true)]
async fn two_tenants_keep_distinct_receipts_under_shared_root_pressure() {
    let (_guard, _, _) = install_deterministic_context(51998);
    let mut state = state_with_specs(&[]);
    let store = temper_store_sim::SimEventStore::no_faults(51998);
    state.set_storage_stack(crate::StorageStack::from_sim(store.clone(), None));
    for (tenant, status) in [("alpha", "Alpha"), ("beta", "Beta")] {
        let source = reaction_spec("Source", &["Child"]).replace("Running", status)
            + "\n[[action.triggers]]\nname=\"gate\"\nkind=\"adapter\"\nadapter=\"tenant-gate\"\n";
        let child = reaction_spec("Child", &[]).replace("Running", status);
        state.registry.write().unwrap().register_tenant(
            tenant,
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Source", &source), ("Child", &child)],
        );
        state
            .authz
            .reload_tenant_policies(tenant, "permit(principal, action, resource);")
            .unwrap();
    }
    state.rebuild_reaction_dispatcher();
    let gate = Arc::new(Gate {
        started: Notify::new(),
        finish: Semaphore::new(0),
        calls: AtomicUsize::new(0),
    });
    let mut adapters = crate::adapters::AdapterRegistry::new();
    adapters.register(gate.clone());
    state.adapter_registry = Arc::new(adapters);
    let _pressure = state
        .idempotency_cache
        .continuation_slots
        .clone()
        .try_acquire_many_owned(62)
        .unwrap();
    let state_ref = &state;
    let invoke = |tenant| async move {
        state_ref
            .dispatch_tenant_action_ext(
                &TenantId::new(tenant),
                "Source",
                "one",
                "Start",
                json!({}),
                DispatchExtOptions {
                    agent_ctx: &AgentContext {
                        idempotency_key: Some("K".into()),
                        ..AgentContext::system()
                    },
                    await_integration: true,
                    await_reactions: true,
                },
            )
            .await
            .unwrap()
    };
    let mut alpha = Box::pin(invoke("alpha"));
    tokio::select! { biased; reply = &mut alpha => panic!("alpha returned early: {reply:?}"), _ = gate.started.notified() => {} }
    let mut beta = Box::pin(invoke("beta"));
    tokio::select! { biased; reply = &mut beta => panic!("beta reused alpha receipt: {reply:?}"), _ = gate.started.notified() => {} }
    assert_eq!(
        state
            .idempotency_cache
            .continuation_slots
            .available_permits(),
        0
    );
    let mut alpha_retry = Box::pin(invoke("alpha"));
    let mut beta_retry = Box::pin(invoke("beta"));
    pending(&mut alpha_retry).await;
    pending(&mut beta_retry).await;
    gate.finish.add_permits(2);
    let (a, b, ar, br) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(alpha, beta, alpha_retry, beta_retry)
    })
    .await
    .unwrap();
    assert_eq!(a.state.status, "Alpha");
    assert_eq!(b.state.status, "Beta");
    assert_eq!(
        serde_json::to_value(a).unwrap(),
        serde_json::to_value(ar).unwrap()
    );
    assert_eq!(
        serde_json::to_value(b).unwrap(),
        serde_json::to_value(br).unwrap()
    );
    for tenant in ["alpha", "beta"] {
        for kind in ["Source", "Child"] {
            let binding = crate::idempotency::request_binding("Start", &json!({}));
            assert!(
                matches!(state.idempotency_cache.lookup_after_completion(
                &format!("{tenant}:{kind}:one"), "K", &binding),
                crate::idempotency::IdempotencyLookup::Hit(response) if response.success),
                "tenant descendant did not reach successful terminal completion under shared pressure"
            );
            assert_eq!(
                store
                    .dump_journal(&format!("{tenant}:{kind}:one"))
                    .iter()
                    .filter(|e| e.payload["action"] == "Start")
                    .count(),
                1
            );
        }
    }
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        state
            .idempotency_cache
            .continuation_slots
            .available_permits(),
        2
    );
}
