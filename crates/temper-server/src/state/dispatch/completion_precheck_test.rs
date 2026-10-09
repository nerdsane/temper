//! Actual WASM/local OData prechecks: shared root capacity and adopted uncertainty.
use super::{
    local_wasm_tests::{CSDL, PARENT, completed_invocations},
    tests::fixture::*,
};
use crate::idempotency::{OperationClaim, OperationPhase, OperationReceipt, OperationRequest};
use crate::registry::{EntityVerificationResult, VerificationStatus};
use crate::{ServerState, StorageStack, request_context::AgentContext, state::DispatchExtOptions};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const WORK: &str = r#"
[automaton]
name="Work"
states=["Idle","Running"]
initial="Idle"
[[action]]
name="Start"
from=["Idle"]
to="Running"
"#;
const RATE: &str = r#"
[automaton]
name="RateLimit"
states=["Active"]
initial="Active"
[[action]]
name="Consume"
from=["Active"]
to="Active"
"#;
const GD: &str = include_str!("../../../../temper-platform/src/specs/GovernanceDecision.ioa.toml");

#[derive(Clone, Copy)]
enum Route {
    Create,
    Bound,
    DeniedPost,
    DeniedGet,
}
impl Route {
    fn denied(self) -> bool {
        matches!(self, Self::DeniedPost | Self::DeniedGet)
    }
}

// Require the real host response's exact status, not just an ABI return or a trap.
fn guest(route: Route, interrupted: bool) -> String {
    let (method, path, status) = match route {
        Route::Create => ("POST", "Works", if interrupted { 500 } else { 201 }),
        Route::Bound => (
            "POST",
            "Works('one')/Temper.Start",
            if interrupted { 500 } else { 200 },
        ),
        Route::DeniedPost => ("POST", "Works('one')/Temper.Start", 403),
        Route::DeniedGet => ("GET", "Works('one')", 403),
    };
    let url = format!("http://localhost:3000/tdata/{path}");
    let body = json!({"Id":"one","OwnerId":"bucket-owner"}).to_string();
    let payload = r#"{"success":true,"params":{}}"#;
    let encode = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|b| format!("\\{b:02x}"))
            .collect::<String>()
    };
    let result = encode(
        &[
            &(payload.len() as u32).to_le_bytes()[..],
            payload.as_bytes(),
        ]
        .concat(),
    );
    let status_word = u32::from_le_bytes(format!("{status}\n").as_bytes().try_into().unwrap());
    format!(
        r#"(module
      (import "env" "host_http_call" (func $http (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 0) "{method}") (data (i32.const 64) "{url}")
      (data (i32.const 1024) "{body}") (data (i32.const 2044) "{result}")
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0 i32.const {method_len} i32.const 64 i32.const {url_len}
        i32.const 0 i32.const 0 i32.const 1024 i32.const {body_len}
        i32.const 4096 i32.const 8192 call $http i32.const 0 i32.gt_s
        if else unreachable end
        i32.const 4096 i32.load i32.const {status_word} i32.eq
        if (result i32) i32.const 2048 else unreachable end))"#,
        body = encode(body.as_bytes()),
        method_len = method.len(),
        url_len = url.len(),
        body_len = body.len()
    )
}

pub(super) fn receipt(
    state: &ServerState,
    tenant: &str,
    kind: &str,
    id: &str,
    key: &str,
) -> Arc<OperationReceipt> {
    let request = OperationRequest {
        tenant: TenantId::new(tenant),
        entity_type: kind.into(),
        entity_id: id.into(),
        action: "unused".into(),
        params: json!({}),
        agent_ctx: AgentContext::system(),
        idempotency_key: Some(key.into()),
        await_integration: false,
        reaction_depth: None,
        detach_reactions: false,
    };
    match state
        .idempotency_cache
        .claim_operation(&format!("{tenant}:{kind}:{id}"), key, request)
    {
        OperationClaim::Join(receipt) => receipt,
        _ => panic!("expected retained adopted receipt for {tenant}:{kind}:{id}"),
    }
}

async fn precheck(route: Route, interrupted: bool) {
    let mut state = state_with_specs(&[("Parent", PARENT), ("Work", WORK), ("RateLimit", RATE)]);
    let store = SimEventStore::no_faults(52000);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let csdl = CSDL.replace(
        "</EntityType>",
        r#"<Property Name="OwnerId" Type="Edm.String"/></EntityType>"#,
    );
    state
        .registry
        .write()
        .unwrap()
        .try_register_tenant_with_constraints(
            "default",
            parse_csdl(&csdl).unwrap(),
            csdl.clone(),
            &[],
            None,
            true,
        )
        .unwrap();
    state.registry.write().unwrap().register_tenant(
        "temper-system",
        parse_csdl(&csdl).unwrap(),
        csdl.clone(),
        &[("GovernanceDecision", GD)],
    );
    state
        .authz
        .reload_tenant_policies("temper-system", "permit(principal, action, resource);")
        .unwrap();
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Work",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "test".into(),
        }),
    );
    let bucket = ServerState::commons_rate_limit_entity_id("bucket-owner", "write");
    state.get_or_create_tenant_entity(&TenantId::default(),"RateLimit",&bucket,json!({"OwnerId":"bucket-owner","ActionClass":"write","Tokens":10,"Capacity":10,"RefillPerSecond":0,"LastRefillAt":"2026-01-01T00:00:00Z"})).await.unwrap();
    if !matches!(route, Route::Create) {
        state
            .get_or_create_tenant_entity(
                &TenantId::default(),
                "Work",
                "one",
                json!({"Id":"one","OwnerId":"bucket-owner"}),
            )
            .await
            .unwrap();
    }
    state
        .commons_guardrail_tenants
        .write()
        .unwrap()
        .insert("default".into());
    if route.denied() {
        state.authz.reload_tenant_policies("default",r#"permit(principal, action, resource); forbid(principal, action in [Action::"Start", Action::"read"], resource);"#).unwrap();
    }
    let (tenant, kind, action) = if route.denied() {
        (
            "temper-system",
            "GovernanceDecision",
            "CreateGovernanceDecision",
        )
    } else {
        ("default", "RateLimit", "Consume")
    };
    if interrupted {
        *state
            .idempotency_cache
            .interrupt_action_once
            .lock()
            .unwrap() = Some((tenant.into(), kind.into(), action.into()));
    }
    let hash = state
        .wasm_engine
        .compile_and_cache(guest(route, interrupted).as_bytes())
        .unwrap();
    state
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&TenantId::default(), "local", &hash);
    // Pressure only in the successful case: before the fix the fault must really
    // reach an adopted worker, rather than being replaced by capacity rejection.
    let _pressure = (!interrupted).then(|| {
        state
            .idempotency_cache
            .continuation_slots
            .clone()
            .try_acquire_many_owned(63)
            .unwrap()
    });
    let mut context = AgentContext::for_service("precheck-invoker");
    context.callback_depth = 2;
    context.callback_hops = 5;
    context.session_id = Some("precheck-session".into());
    context
        .security_ctx
        .as_mut()
        .unwrap()
        .context_attrs
        .insert("sessionId".into(), json!("precheck-session"));
    let invoke = |key| {
        let mut ctx = context.clone();
        ctx.idempotency_key = key;
        let state = &state;
        async move {
            state
                .dispatch_tenant_action_ext(
                    &TenantId::default(),
                    "Parent",
                    "one",
                    "Start",
                    json!({}),
                    DispatchExtOptions {
                        agent_ctx: &ctx,
                        await_integration: true,
                        await_reactions: true,
                    },
                )
                .await
                .unwrap()
        }
    };
    let response = tokio::time::timeout(Duration::from_secs(5), invoke(None))
        .await
        .unwrap();
    let root_event = store
        .dump_journal("default:Parent:one")
        .into_iter()
        .find(|e| e.payload["action"] == "Start")
        .unwrap();
    let parent_key = root_event.payload["idempotency_key"].as_str().unwrap();
    let invocations = completed_invocations(&state);
    assert_eq!(invocations.len(), 1);
    assert_eq!(
        invocations[0]["result"], "success",
        "guest did not receive the expected ordinary HTTP status: {invocations:?}"
    );
    if interrupted {
        assert!(
            state
                .idempotency_cache
                .interrupt_action_once
                .lock()
                .unwrap()
                .is_none(),
            "adopted worker fault was not reached"
        );
        assert!(
            !response.success,
            "ordinary HTTP response concealed an Interrupted precheck: {response:?}"
        );
        assert!(response.error.unwrap().contains("completion is unknown"));
        assert_eq!(action_count(&store, "Parent", "Complete"), 0);
        assert_eq!(action_count(&store, "Parent", "Fail"), 0);
        assert_eq!(invocations[0]["operation_completion"], "unknown");
        assert!(!invoke(Some(parent_key.to_owned())).await.success);
        assert_eq!(
            completed_invocations(&state).len(),
            1,
            "unknown root executed a fresh attempt"
        );
        assert_eq!(
            state
                .idempotency_cache
                .continuation_slots
                .available_permits(),
            63
        );
    } else {
        assert!(response.success, "{response:?}");
        assert_eq!(response.state.status, "Done");
        assert_eq!(invocations[0]["operation_completion"], "known");
        assert_eq!(action_count(&store, "Parent", "Complete"), 1);
        assert_eq!(
            state
                .idempotency_cache
                .continuation_slots
                .available_permits(),
            1
        );
    }
    let id = if route.denied() {
        store
            .list_entity_ids_by_type(tenant, kind)
            .await
            .unwrap()
            .into_iter()
            .next()
            .expect("audit entity")
    } else {
        bucket
    };
    let event = store
        .dump_journal(&format!("{tenant}:{kind}:{id}"))
        .into_iter()
        .find(|e| e.payload["action"] == action)
        .unwrap();
    let key = event.payload["idempotency_key"].as_str().unwrap();
    assert_ne!(key, parent_key);
    let child = receipt(&state, tenant, kind, &id, key);
    assert_eq!(
        child.phase(),
        if interrupted {
            OperationPhase::Interrupted
        } else {
            OperationPhase::Terminal
        }
    );
    let ctx = &child.request.agent_ctx;
    let service = if route.denied() {
        "platform-dispatch"
    } else {
        "commons-rate-limit"
    };
    assert_eq!(
        ctx.agent_id.as_deref(),
        Some(format!("service:{service}").as_str())
    );
    assert_eq!(
        ctx.security_ctx.as_ref().unwrap().principal.id,
        format!("service:{service}")
    );
    assert_eq!(
        ctx.callback_hops,
        if matches!(route, Route::DeniedGet) {
            5
        } else {
            6
        }
    );
    assert_eq!(
        ctx.callback_depth,
        if matches!(route, Route::DeniedGet) {
            2
        } else {
            3
        }
    );
    assert!(ctx.has_effects_ancestor("default:Parent:one", parent_key));
    if !route.denied() {
        assert!(key.starts_with("commons-rate-limit:default:"));
        assert!(!child.request.await_integration);
        assert!(child.request.reaction_depth.is_none());
    }
    assert_eq!(
        action_count(&store, "Work", "Start"),
        usize::from(matches!(route, Route::Bound) && !interrupted)
    );
    if matches!(route, Route::Create) {
        assert_eq!(
            state.entity_exists(&TenantId::default(), "Work", "one"),
            !interrupted
        );
    }
}

use temper_runtime::persistence::EventStore;
#[tokio::test]
async fn collection_precheck_shares_last_root_slot() {
    precheck(Route::Create, false).await;
}
#[tokio::test]
async fn bound_precheck_shares_last_root_slot() {
    precheck(Route::Bound, false).await;
}
#[tokio::test]
async fn collection_precheck_interrupted_receipt_survives_http_error() {
    precheck(Route::Create, true).await;
}
#[tokio::test]
async fn bound_precheck_interrupted_receipt_survives_http_error() {
    precheck(Route::Bound, true).await;
}
#[tokio::test]
async fn denied_post_audit_shares_last_root_slot() {
    precheck(Route::DeniedPost, false).await;
}
#[tokio::test]
async fn denied_post_audit_interrupted_receipt_survives_denial() {
    precheck(Route::DeniedPost, true).await;
}
#[tokio::test]
async fn denied_get_audit_shares_last_root_slot() {
    precheck(Route::DeniedGet, false).await;
}
#[tokio::test]
async fn denied_get_audit_interrupted_receipt_survives_denial() {
    precheck(Route::DeniedGet, true).await;
}
