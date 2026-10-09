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

#[test]
fn effects_ancestors_follow_inline_contexts_but_not_detached_or_http_requests() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(51951);
    let parent = AgentContext::system()
        .with_effects_ancestor("tenant:Task:one", "K")
        .unwrap();
    let callback = parent
        .for_callback()
        .unwrap()
        .for_dispatch_root("Task", "one");
    assert!(callback.has_effects_ancestor("tenant:Task:one", "K"));
    let nested = callback
        .with_effects_ancestor("tenant:Task:two", "child")
        .unwrap();
    assert!(nested.has_effects_ancestor("tenant:Task:one", "K"));
    assert!(nested.has_effects_ancestor("tenant:Task:two", "child"));
    assert_eq!(callback.effects_ancestors.len(), 1);
    assert!(!callback.has_effects_ancestor("other-tenant:Task:one", "K"));
    assert!(!callback.has_effects_ancestor("tenant:Task:one", "different-key"));
    for service in [
        "platform-dispatch",
        "wasm-runtime",
        "integration-compensation",
    ] {
        let inherited = AgentContext::for_service_inheriting(service, &callback);
        assert!(inherited.has_effects_ancestor("tenant:Task:one", "K"));
        assert!(inherited.for_background_task().effects_ancestors.is_empty());
    }
    // Narrower identities use this same inheritance path without copying keys.
    let inherited = AgentContext::default().inherit_observability_from(&callback);
    assert!(inherited.has_effects_ancestor("tenant:Task:one", "K"));
    assert!(
        parent.effects_ancestors.len() == 1,
        "cloned child must not mutate its parent"
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-temper-effects-ancestors",
        "tenant:Task:one:K".parse().unwrap(),
    );
    assert!(extract_agent_context(&headers).effects_ancestors.is_empty());
}

#[test]
fn effects_ancestor_budget_refuses_without_extending_the_lineage() {
    let mut context = AgentContext::default();
    for n in 0..MAX_CALLBACK_HOPS {
        context = context
            .with_effects_ancestor("tenant:Task:one", &n.to_string())
            .unwrap();
    }
    assert_eq!(
        context
            .with_effects_ancestor("tenant:Task:one", "overflow")
            .unwrap_err(),
        "post-dispatch effects ancestor budget exhausted"
    );
    assert_eq!(context.effects_ancestors.len(), MAX_CALLBACK_HOPS as usize);
}

#[test]
fn detached_effect_clears_only_ancestry_preserving_all_other_context() {
    let parent = AgentContext {
        callback_depth: 7,
        callback_hops: MAX_CALLBACK_HOPS - 1,
        idempotency_key: Some("same-key".into()),
        effects_ancestors: vec![("tenant:Task:one".into(), "same-key".into())],
        session_id: Some("session".into()),
        trace_id: Some("trace".into()),
        parent_span_id: Some("parent-span".into()),
        workflow_root_entity_type: Some("Task".into()),
        workflow_root_entity_id: Some("one".into()),
        workflow_run_id: Some("workflow".into()),
        intent: Some("detached effect".into()),
        observation_metadata: BTreeMap::from([("test.origin".into(), "parent".into())]),
        ..AgentContext::system()
    };
    let detached = parent.without_effects_ancestors();
    assert!(detached.effects_ancestors.is_empty());
    assert_eq!(detached.callback_depth, 7);
    assert_eq!(detached.callback_hops, MAX_CALLBACK_HOPS - 1);
    assert_eq!(detached.idempotency_key, parent.idempotency_key);
    // This context has no owned operation. Restoring its ancestors reproduces
    // all identity, attribution, tracing and callback-budget fields.
    let restored = AgentContext {
        effects_ancestors: parent.effects_ancestors.clone(),
        ..detached
    };
    assert_eq!(format!("{restored:?}"), format!("{parent:?}"));
}

#[test]
fn completion_evidence_and_root_capacity_follow_only_joined_contexts() {
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let permit = std::sync::Arc::new(semaphore.clone().try_acquire_owned().unwrap());
    let parent = AgentContext {
        completion_capacity: Some(std::sync::Arc::downgrade(&permit)),
        ..AgentContext::system()
    };
    parent.local_completion.mark_unknown();
    let child = AgentContext::default().inherit_observability_from(&parent);
    assert!(child.local_completion.is_unknown());
    assert!(child.completion_capacity.unwrap().upgrade().is_some());
    let detached = parent.for_background_task();
    assert!(!detached.local_completion.is_unknown());
    assert!(detached.completion_capacity.is_none());
}
