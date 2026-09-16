//! Deterministic scheduling of the production callback context transitions.
//!
//! A seed chooses inline callbacks, detached successes and compensation
//! identities. Identity provisioning is a deterministic fixture: the production
//! service constructor generates an unrelated correlation UUID. The budget,
//! task boundary and context inheritance methods run unmodified. Real WASM and
//! compensation wiring is exercised separately by strict_native_callbacks.
use super::*;
use temper_runtime::scheduler::DeterministicRng;

#[test]
fn dst_detached_schedules_preserve_both_callback_bounds() {
    let mut coverage = [0_u32; 4];
    for seed in 1..=1024 {
        let mut rng = DeterministicRng::new(seed);
        let mut context = AgentContext {
            session_id: Some("session".into()),
            workflow_run_id: Some("workflow".into()),
            intent: Some("bounded workflow".into()),
            observation_metadata: BTreeMap::from([("test.seed".into(), seed.to_string())]),
            ..AgentContext::default()
        };
        // The oracle is the approved 512-hop contract, independent of the implementation constant.
        for admitted in 1..=512 {
            let operation = rng.next_bound(4);
            coverage[operation] += 1;
            if context.callback_depth == temper_runtime::reaction::MAX_REACTION_DEPTH {
                assert_eq!(
                    context.for_callback().unwrap_err(),
                    CallbackBudgetExceeded::InlineDepth,
                    "seed={seed}, admitted={admitted}"
                );
                context = context.for_background_task();
            }
            if operation != 0 {
                let previous_hops = context.callback_hops;
                context = context.for_background_task();
                assert_eq!(context.callback_depth, 0, "seed={seed}");
                assert_eq!(context.callback_hops, previous_hops, "seed={seed}");
                let service = match operation {
                    1 => "wasm-runtime",
                    2 => "platform-dispatch",
                    3 => "integration-compensation",
                    _ => unreachable!("bounded operation"),
                };
                context = service_identity(service).inherit_observability_from(&context);
                assert_eq!(context.agent_type.as_deref(), Some(service), "seed={seed}");
            }
            let previous_depth = context.callback_depth;
            context = context.for_callback().unwrap_or_else(|error| {
                panic!("seed={seed}, admitted={admitted}, operation={operation}: {error}")
            });
            assert_eq!(context.callback_hops, admitted, "seed={seed}");
            assert_eq!(context.callback_depth, previous_depth + 1, "seed={seed}");
            assert_eq!(context.session_id.as_deref(), Some("session"));
            assert_eq!(context.workflow_run_id.as_deref(), Some("workflow"));
            assert_eq!(context.intent.as_deref(), Some("bounded workflow"));
            assert_eq!(context.observation_metadata["test.seed"], seed.to_string());
        }
        assert_total_hops_exhausted(context, seed);
    }
    assert!(coverage.iter().all(|count| *count > 0), "{coverage:?}");
}

// Provisioning identity is outside the callback-budget state machine. The
// production preservation method below is the one used by for_service_inheriting.
fn service_identity(service: &str) -> AgentContext {
    AgentContext {
        agent_id: Some(format!("service:{service}")),
        agent_type: Some(service.into()),
        ..AgentContext::default()
    }
}

fn assert_total_hops_exhausted(mut context: AgentContext, seed: u64) {
    for service in [
        "wasm-runtime",
        "platform-dispatch",
        "integration-compensation",
    ] {
        context = context.for_background_task();
        context = service_identity(service).inherit_observability_from(&context);
        assert_eq!(context.callback_hops, 512, "seed={seed}");
        assert_eq!(
            context.for_callback().unwrap_err(),
            CallbackBudgetExceeded::TotalHops,
            "seed={seed}, service={service}"
        );
    }
}

#[test]
fn inline_callbacks_exhaust_without_a_detached_boundary() {
    let mut context = AgentContext::default();
    for admitted in 1..=8 {
        context = context.for_callback().unwrap();
        assert_eq!(context.callback_depth, admitted);
        assert_eq!(context.callback_hops, admitted);
    }
    let inherited = AgentContext::for_service_inheriting("wasm-runtime", &context);
    assert_eq!(
        inherited.for_callback().unwrap_err(),
        CallbackBudgetExceeded::InlineDepth
    );
    assert_eq!(inherited.callback_hops, context.callback_hops);
}

#[test]
fn request_headers_cannot_supply_callback_budgets() {
    let mut headers = HeaderMap::new();
    for name in [
        "x-callback-depth",
        "x-callback-hops",
        "x-temper-callback-depth",
        "x-temper-callback-hops",
    ] {
        headers.insert(name, axum::http::HeaderValue::from_static("511"));
    }
    let context = extract_agent_context(&headers);
    assert_eq!(context.callback_depth, 0);
    assert_eq!(context.callback_hops, 0);
    assert!(context.security_ctx.is_none());
}

#[test]
fn detached_boundary_preserves_authority_and_idempotency() {
    let context = AgentContext {
        callback_depth: 8,
        callback_hops: 40,
        idempotency_key: Some("request-key".into()),
        ..AgentContext::for_service("invoker")
    };
    let detached = context.for_background_task();
    assert_eq!(detached.callback_depth, 0);
    assert_eq!(detached.callback_hops, 40);
    assert_eq!(detached.agent_id, context.agent_id);
    assert_eq!(detached.agent_type, context.agent_type);
    assert_eq!(detached.idempotency_key, context.idempotency_key);
    assert_eq!(
        serde_json::to_value(&detached.security_ctx).unwrap(),
        serde_json::to_value(&context.security_ctx).unwrap()
    );
}
