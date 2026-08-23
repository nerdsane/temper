//! Unit tests for WASM dispatch, including the ADR-0166 callback-param gate.
use super::*;

#[test]
fn internal_http_issuer_refuses_system_and_accepts_resolved_agents() {
    let state = crate::state::ServerState::from_registry(
        temper_runtime::ActorSystem::new("internal-capability-issuer-test"),
        crate::registry::SpecRegistry::new(),
    );
    let tenant = TenantId::new("tenant-a");

    assert!(
        internal_http_capability_issuer(&state, &tenant, Some(&SecurityContext::system()))
            .is_none()
    );
    let agent = SecurityContext::from_resolved_identity("agent-1", "worker", None);
    assert!(internal_http_capability_issuer(&state, &tenant, Some(&agent)).is_some());
}

#[test]
fn internal_wasm_http_issuer_preserves_the_module_identity() {
    let state = crate::state::ServerState::from_registry(
        temper_runtime::ActorSystem::new("internal-wasm-capability-issuer-test"),
        crate::registry::SpecRegistry::new(),
    );
    let tenant = TenantId::new("tenant-a");
    let wasm = WasmAuthzContext {
        tenant: tenant.to_string(),
        module_name: "advance_arc_task_synthesis".to_string(),
        agent_id: Some("service:wasm-runtime".to_string()),
        session_id: None,
        entity_type: "ArcTaskSynthesis".to_string(),
        trigger_action: "RecordInitialSubmitted".to_string(),
    };
    let issuer = internal_wasm_http_capability_issuer(&state, &tenant, &wasm);
    let capability = issuer(
        "POST",
        "http://127.0.0.1:3000/api/v1/schema-deployments/scope/digest/verify",
    )
    .expect("module-bound capability should issue");
    let authenticated = state
        .internal_invocation_credentials
        .consume_for_request(
            capability.bearer_token(),
            &tenant,
            &axum::http::Method::POST,
            &"/api/v1/schema-deployments/scope/digest/verify"
                .parse()
                .expect("request target should parse"),
        )
        .expect("module-bound capability should resolve");

    assert_eq!(
        authenticated.security_context().principal.id,
        "advance_arc_task_synthesis"
    );
    assert_eq!(
        authenticated.security_context().principal.role.as_deref(),
        Some("wasm_module")
    );
    assert_eq!(
        authenticated.security_context().context_attrs.get("module"),
        Some(&json!("advance_arc_task_synthesis"))
    );
}

#[test]
fn composite_wasm_result_inherits_generated_dispatch_idempotency() {
    let agent = AgentContext::for_service("version-publisher");

    let composite_agent = agent_ctx_for_composite_wasm_result(
        &agent,
        Some("dispatch:default:App:app:PublishNewVersion:one"),
    );

    assert_eq!(
        composite_agent.idempotency_key.as_deref(),
        Some("dispatch:default:App:app:PublishNewVersion:one"),
        "composite sub-writes need the parent dispatch idempotency so repeated app version updates get distinct sub-write keys"
    );
}

#[test]
fn composite_wasm_result_preserves_caller_supplied_idempotency() {
    let mut agent = AgentContext::for_service("version-publisher");
    agent.idempotency_key = Some("caller-key".to_string());

    let composite_agent = agent_ctx_for_composite_wasm_result(&agent, Some("generated-key"));

    assert_eq!(
        composite_agent.idempotency_key.as_deref(),
        Some("caller-key"),
        "caller idempotency remains authoritative for retries"
    );
}

#[test]
fn gen_ai_span_attrs_are_recorded_only_for_llm_integrations() {
    let params = json!({
        "input_tokens": 10,
        "output_tokens": 20,
        "_gen_ai_input_messages": "[{\"role\":\"user\"}]",
        "_gen_ai_output_messages": "[{\"role\":\"assistant\"}]",
        "_gen_ai_provider": "openai",
        "_gen_ai_model": "gpt-5.4",
    });

    assert!(should_record_gen_ai_span_attrs(true, &params));
    assert!(!should_record_gen_ai_span_attrs(false, &params));
}

#[test]
fn llmobs_service_name_prefers_runtime_service_identity() {
    unsafe {
        std::env::set_var("DD_SERVICE", "temperpaw");
        std::env::remove_var("OTEL_SERVICE_NAME");
    }
    assert_eq!(llmobs_service_name(), "temperpaw");

    unsafe {
        std::env::remove_var("DD_SERVICE");
        std::env::set_var("OTEL_SERVICE_NAME", "temper-agent");
    }
    assert_eq!(llmobs_service_name(), "temper-agent");

    unsafe {
        std::env::remove_var("DD_SERVICE");
        std::env::remove_var("OTEL_SERVICE_NAME");
    }
    assert_eq!(llmobs_service_name(), "temper-platform");
}

#[test]
fn llm_model_for_observability_prefers_callback_model() {
    let entity_state = EntityState {
        entity_type: "Session".to_string(),
        entity_id: "session-1".to_string(),
        status: "CallingProvider".to_string(),
        item_count: 0,
        counters: std::collections::BTreeMap::new(),
        booleans: std::collections::BTreeMap::new(),
        lists: std::collections::BTreeMap::new(),
        fields: json!({"model": "claude-sonnet-4-6"}),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
    };
    let callback_params = json!({
        "_gen_ai_model": "gpt-5.4",
    });

    assert_eq!(
        llm_model_for_observability(&entity_state, &callback_params),
        "gpt-5.4"
    );
}

#[test]
fn parse_internal_file_value_request_matches_only_value_paths() {
    assert_eq!(
        parse_internal_file_value_request(
            "http://127.0.0.1:3467",
            "http://127.0.0.1:3467/tdata/Files('fl-123')/$value"
        )
        .as_deref(),
        Some("fl-123")
    );
    assert!(
        parse_internal_file_value_request(
            "http://127.0.0.1:3467",
            "http://127.0.0.1:3467/tdata/Files('fl-123')"
        )
        .is_none()
    );
}

#[test]
fn llm_root_span_stays_on_active_trace() {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::prelude::*;

    let tracer_provider = SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(tracer_provider.tracer("temper-server-llm-root-test")),
    );
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let tenant = TenantId::default();
    let entity_state = EntityState {
        entity_type: "Session".to_string(),
        entity_id: "ss-1".to_string(),
        status: "CallingProvider".to_string(),
        item_count: 0,
        counters: std::collections::BTreeMap::new(),
        booleans: std::collections::BTreeMap::new(),
        lists: std::collections::BTreeMap::new(),
        fields: json!({"provider": "openai", "model": "gpt-5.4"}),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
    };
    let integration = temper_spec::automaton::Integration {
        name: "provider_caller".to_string(),
        trigger: "call_provider".to_string(),
        integration_type: "wasm".to_string(),
        module: Some("provider_caller".to_string()),
        config: std::collections::BTreeMap::new(),
        on_success: None,
        on_failure: None,
        llm: true,
    };
    let agent_ctx = AgentContext {
        session_id: Some("ss-1".to_string()),
        ..AgentContext::default()
    };

    let parent = tracing::info_span!("dispatch.dispatch_tenant_action_core");
    let expected_trace_id = parent.in_scope(|| {
        tracing::Span::current()
            .context()
            .span()
            .span_context()
            .trace_id()
            .to_string()
    });
    let (llm_trace_id, has_llmobs_auto_conversion_opt_out) = parent.in_scope(|| {
        let ctx = WasmDispatchCtx {
            entity_ref: WasmEntityRef {
                tenant: &tenant,
                entity_type: "Session",
                entity_id: "ss-1",
            },
            action: "ContextReady",
            agent_ctx: &agent_ctx,
            dispatch_idempotency_key: None,
            mode: WasmDispatchMode::Inline,
        };
        let span = build_llm_root_span(&ctx, &integration, &entity_state, "provider_caller");
        let has_opt_out = span
            .metadata()
            .map(|metadata| {
                metadata
                    .fields()
                    .iter()
                    .any(|field| field.name() == "dd_llmobs_enabled")
            })
            .unwrap_or(false);
        (
            span.context().span().span_context().trace_id().to_string(),
            has_opt_out,
        )
    });

    assert_eq!(llm_trace_id, expected_trace_id);
    assert!(
        has_llmobs_auto_conversion_opt_out,
        "root LLM OTel span must opt out of Datadog auto LLMObs conversion"
    );
}

#[test]
fn llm_parent_context_records_llm_span_and_dispatch_parent_ids() {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::prelude::*;

    let tracer_provider = SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(tracer_provider.tracer("temper-server-llm-parent-test")),
    );
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let dispatch_parent = tracing::info_span!("dispatch.dispatch_tenant_action_core");
    let (expected_trace_id, expected_parent_span_id) = dispatch_parent.in_scope(|| {
        let span_context = tracing::Span::current()
            .context()
            .span()
            .span_context()
            .clone();
        (
            span_context.trace_id().to_string(),
            span_context.span_id().to_string(),
        )
    });

    let mut callback_params = json!({});
    let entity_state = EntityState {
        entity_type: "Session".to_string(),
        entity_id: "session-1".to_string(),
        status: "CallingProvider".to_string(),
        item_count: 0,
        counters: std::collections::BTreeMap::new(),
        booleans: std::collections::BTreeMap::new(),
        lists: std::collections::BTreeMap::new(),
        fields: json!({}),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
    };
    let (llm_trace_id, llm_span_id) = dispatch_parent.in_scope(|| {
        let llm_span = tracing::info_span!("llm_caller.trace");
        let span_context = llm_span.context().span().span_context().clone();
        attach_llm_parent_context(
            &llm_span,
            Some(&expected_parent_span_id),
            &entity_state,
            "session-1",
            1_234,
            &mut callback_params,
        );
        (
            span_context.trace_id().to_string(),
            span_context.span_id().to_string(),
        )
    });

    assert_eq!(llm_trace_id, expected_trace_id);
    assert_ne!(llm_span_id, expected_parent_span_id);
    assert_eq!(
        callback_params["_gen_ai_parent_trace_id"],
        expected_trace_id
    );
    assert_eq!(callback_params["_gen_ai_parent_span_id"], llm_span_id);
    assert_eq!(
        callback_params["_gen_ai_llm_parent_span_id"],
        expected_parent_span_id
    );
    assert_eq!(
        callback_params["gen_ai_llm_parent_span_id"],
        expected_parent_span_id
    );
    let expected_agent_span_id =
        temper_observe::llmobs_api::derive_span_id(&format!("{expected_trace_id}:session-1:agent"));
    assert_eq!(
        callback_params["_gen_ai_llmobs_agent_span_id"],
        expected_agent_span_id
    );
    assert_eq!(
        callback_params["llmobs_agent_span_id"],
        expected_agent_span_id
    );
    assert_ne!(
        callback_params["_gen_ai_llmobs_agent_span_id"],
        expected_parent_span_id
    );
    assert!(
        callback_params["_gen_ai_llmobs_workflow_span_id"]
            .as_str()
            .is_some_and(|workflow_span_id| !workflow_span_id.is_empty()
                && workflow_span_id != expected_parent_span_id
                && workflow_span_id != llm_span_id)
    );
    assert_eq!(
        callback_params["llmobs_workflow_span_id"],
        callback_params["_gen_ai_llmobs_workflow_span_id"]
    );
    assert!(
        callback_params["llmobs_agent_start_ns"]
            .as_u64()
            .is_some_and(|start_ns| start_ns > 0)
    );
}

#[test]
fn llm_parent_context_reuses_existing_llmobs_agent_root() {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::prelude::*;

    let tracer_provider = SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(tracer_provider.tracer("temper-server-llm-parent-reuse-test")),
    );
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let entity_state = EntityState {
        entity_type: "Session".to_string(),
        entity_id: "session-1".to_string(),
        status: "CallingProvider".to_string(),
        item_count: 0,
        counters: std::collections::BTreeMap::new(),
        booleans: std::collections::BTreeMap::new(),
        lists: std::collections::BTreeMap::new(),
        fields: json!({
            "llmobs_agent_span_id": "stable-agent-root",
            "llmobs_agent_start_ns": 12345_u64,
        }),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
    };

    let mut callback_params = json!({});
    let llm_span = tracing::info_span!("llm_caller.trace");
    attach_llm_parent_context(
        &llm_span,
        Some("turn-parent-span"),
        &entity_state,
        "session-1",
        99,
        &mut callback_params,
    );

    assert_eq!(
        callback_params["_gen_ai_llmobs_agent_span_id"],
        "stable-agent-root"
    );
    assert_eq!(callback_params["llmobs_agent_span_id"], "stable-agent-root");
    assert_eq!(callback_params["_gen_ai_llmobs_agent_start_ns"], 12345_u64);
    assert_eq!(callback_params["llmobs_agent_start_ns"], 12345_u64);
}

#[test]
fn llmobs_tool_parent_prefers_workflow_span_id() {
    let entity_state = EntityState {
        entity_type: "Session".to_string(),
        entity_id: "ss-1".to_string(),
        status: "CallingTools".to_string(),
        item_count: 0,
        counters: std::collections::BTreeMap::new(),
        booleans: std::collections::BTreeMap::new(),
        lists: std::collections::BTreeMap::new(),
        fields: json!({
            "gen_ai_parent_trace_id": "trace-1",
            "gen_ai_parent_span_id": "legacy-llm-parent",
            "llmobs_workflow_span_id": "workflow-parent",
        }),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
    };

    assert_eq!(
        llmobs_tool_trace_and_parent(&entity_state, &json!({})),
        Some(("trace-1".to_string(), "workflow-parent".to_string()))
    );
}
