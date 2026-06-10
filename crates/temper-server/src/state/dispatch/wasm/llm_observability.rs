//! LLM observability helpers for WASM dispatch.
//!
//! Covers `gen_ai.*` span attribute recording, LLM wide-event construction,
//! Datadog LLM Observability span submission, and error annotation on
//! dispatch spans. Everything here is observability-only and never alters
//! dispatch behavior.

use std::time::{SystemTime, UNIX_EPOCH};

use opentelemetry::trace::{Status, TraceContextExt};
use serde_json::{Value, json};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::entity_actor::EntityState;

use super::WasmDispatchCtx;

fn llmobs_service_name() -> String {
    for var in ["DD_SERVICE", "OTEL_SERVICE_NAME"] {
        let Some(value) = std::env::var(var) // determinism-ok: observability-only process config
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        return value;
    }
    "temper-platform".to_string()
}

pub(super) fn llm_call_wide_event<'a>(
    ctx: &'a WasmDispatchCtx<'a>,
    entity_state: &'a EntityState,
    callback_params: &'a Value,
    duration_ms: u64,
) -> temper_observe::wide_event::WideEvent {
    let provider = llm_provider_for_observability(entity_state, callback_params);
    let model = llm_model_for_observability(entity_state, callback_params);
    let session_id = ctx
        .agent_ctx
        .session_id
        .as_deref()
        .unwrap_or(ctx.entity_ref.entity_id);
    let input_tokens = callback_params
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output_tokens = callback_params
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let stop_reason = callback_params
        .get("_gen_ai_finish_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input_messages = callback_params
        .get("_gen_ai_input_messages")
        .and_then(Value::as_str);
    let output_messages = callback_params
        .get("_gen_ai_output_messages")
        .and_then(Value::as_str);
    let system_instructions = callback_params
        .get("_gen_ai_system_instructions")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let trace_id = current_otel_trace_id(&Span::current())
        .or_else(|| ctx.agent_ctx.trace_id.clone())
        .unwrap_or_default();

    temper_observe::wide_event::from_llm_call(temper_observe::wide_event::LlmCallInput {
        provider: &provider,
        model: &model,
        operation: "chat",
        entity_type: ctx.entity_ref.entity_type,
        entity_id: ctx.entity_ref.entity_id,
        session_id,
        success: true,
        duration_ns: duration_ms * 1_000_000,
        input_tokens,
        output_tokens,
        stop_reason,
        system_instructions,
        input_messages,
        output_messages,
        trace_id: &trace_id,
        error: None,
    })
}

pub(super) fn should_record_gen_ai_span_attrs(
    integration_is_llm: bool,
    _callback_params: &Value,
) -> bool {
    integration_is_llm
}

pub(super) fn llm_provider_for_observability(
    entity_state: &EntityState,
    callback_params: &Value,
) -> String {
    let provider = callback_params
        .get("_gen_ai_provider")
        .and_then(Value::as_str)
        .or_else(|| entity_state.fields.get("provider").and_then(Value::as_str))
        .unwrap_or("unknown");
    normalize_llm_provider_for_observability(provider)
}

fn normalize_llm_provider_for_observability(provider: &str) -> String {
    let trimmed = provider.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "openai_codex" => "openai".to_string(),
        "mock" => "custom".to_string(),
        "" => "unknown".to_string(),
        _ => trimmed.to_string(),
    }
}

pub(super) fn llm_model_for_observability(
    entity_state: &EntityState,
    callback_params: &Value,
) -> String {
    callback_params
        .get("_gen_ai_model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            entity_state
                .fields
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or("unknown")
        .trim()
        .to_string()
}

pub(super) async fn submit_llmobs_llm_span(
    ctx: &WasmDispatchCtx<'_>,
    entity_state: &EntityState,
    callback_params: &Value,
    duration_ms: u64,
    module_name: &str,
) {
    let current_trace_id = current_otel_trace_id(&Span::current());
    let trace_id = callback_params
        .get("_gen_ai_parent_trace_id")
        .and_then(Value::as_str)
        .or(current_trace_id.as_deref());
    let span_id = callback_params
        .get("_gen_ai_parent_span_id")
        .and_then(Value::as_str);
    let parent_span_id = callback_params
        .get("_gen_ai_llm_parent_span_id")
        .and_then(Value::as_str)
        .or(ctx.agent_ctx.parent_span_id.as_deref());
    let (Some(trace_id), Some(span_id)) = (trace_id, span_id) else {
        return;
    };

    let provider = llm_provider_for_observability(entity_state, callback_params);
    let model = llm_model_for_observability(entity_state, callback_params);
    let session_id = ctx
        .agent_ctx
        .session_id
        .as_deref()
        .unwrap_or(ctx.entity_ref.entity_id);
    let span_name = format!("wasm:{module_name}");
    let workflow_name = format!("{}.{}", ctx.entity_ref.entity_type, ctx.action);
    let agent_span_id = callback_params
        .get("_gen_ai_llmobs_agent_span_id")
        .and_then(Value::as_str)
        .or(parent_span_id);
    let workflow_span_id = callback_params
        .get("_gen_ai_llmobs_workflow_span_id")
        .and_then(Value::as_str);

    let input_tokens = callback_params
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output_tokens = callback_params
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let service_name = llmobs_service_name();

    if let Err(error) =
        temper_observe::llmobs_api::submit_llm_span(temper_observe::llmobs_api::LlmSpanInput {
            service_name: &service_name,
            session_id,
            trace_id,
            span_id,
            parent_span_id,
            agent_span_id,
            agent_start_ns: llmobs_agent_start_ns(entity_state, callback_params),
            workflow_span_id,
            agent_name: Some("temperpaw.agent.session"),
            workflow_name: Some(&workflow_name),
            span_name: &span_name,
            provider: &provider,
            model: &model,
            system_instructions: callback_params
                .get("_gen_ai_system_instructions")
                .and_then(Value::as_str),
            input_messages_json: callback_params
                .get("_gen_ai_input_messages")
                .and_then(Value::as_str),
            output_messages_json: callback_params
                .get("_gen_ai_output_messages")
                .and_then(Value::as_str),
            input_tokens,
            output_tokens,
            finish_reason: callback_params
                .get("_gen_ai_finish_reason")
                .and_then(Value::as_str),
            duration_ms,
            error_type: None,
        })
        .await
    {
        tracing::warn!(
            tenant = %ctx.entity_ref.tenant,
            entity_id = ctx.entity_ref.entity_id,
            session_id,
            %error,
            "failed to submit llm span to Datadog LLM Observability API"
        );
    }
}

pub(super) async fn submit_llmobs_tool_spans(
    ctx: &WasmDispatchCtx<'_>,
    entity_state: &EntityState,
    callback_params: &Value,
) {
    let raw_events = callback_params
        .get("_dd_llmobs_tool_spans")
        .and_then(Value::as_array);
    let Some(raw_events) = raw_events else {
        return;
    };

    let Some((trace_id, parent_span_id)) =
        llmobs_tool_trace_and_parent(entity_state, callback_params)
    else {
        tracing::warn!(
            tenant = %ctx.entity_ref.tenant,
            entity_id = ctx.entity_ref.entity_id,
            raw_event_count = raw_events.len(),
            "skipping Datadog LLMObs tool span submission because parent trace context is missing"
        );
        return;
    };
    let session_id = ctx
        .agent_ctx
        .session_id
        .as_deref()
        .unwrap_or(ctx.entity_ref.entity_id);
    let service_name = llmobs_service_name();

    let spans: Vec<_> = raw_events
        .iter()
        .filter_map(|event| {
            let tool_name = event.get("tool_name").and_then(Value::as_str)?;
            let tool_call_id = event.get("tool_call_id").and_then(Value::as_str)?;
            let arguments = event
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let result_text = event.get("result").and_then(Value::as_str).unwrap_or("");
            let duration_ms = event
                .get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let is_error = event
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(temper_observe::llmobs_api::ToolSpanInput {
                service_name: &service_name,
                session_id,
                trace_id: &trace_id,
                parent_span_id: &parent_span_id,
                tool_name,
                tool_call_id,
                arguments_json: arguments,
                result_text,
                duration_ms,
                is_error,
            })
        })
        .collect();

    if spans.is_empty() {
        return;
    }

    if let Err(error) = temper_observe::llmobs_api::submit_tool_spans(
        &service_name,
        session_id,
        &trace_id,
        &parent_span_id,
        &spans,
    )
    .await
    {
        tracing::warn!(
            tenant = %ctx.entity_ref.tenant,
            entity_id = ctx.entity_ref.entity_id,
            session_id,
            %error,
            "failed to submit tool spans to Datadog LLM Observability API"
        );
    }
}

fn llmobs_tool_trace_and_parent(
    entity_state: &EntityState,
    callback_params: &Value,
) -> Option<(String, String)> {
    let trace_id = entity_state
        .fields
        .get("gen_ai_parent_trace_id")
        .and_then(Value::as_str)
        .or_else(|| {
            entity_state
                .fields
                .get("_gen_ai_parent_trace_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            callback_params
                .get("_gen_ai_parent_trace_id")
                .and_then(Value::as_str)
        })?;
    let parent_span_id = entity_state
        .fields
        .get("llmobs_workflow_span_id")
        .and_then(Value::as_str)
        .or_else(|| {
            entity_state
                .fields
                .get("_gen_ai_llmobs_workflow_span_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            callback_params
                .get("_gen_ai_llmobs_workflow_span_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            entity_state
                .fields
                .get("gen_ai_parent_span_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            entity_state
                .fields
                .get("_gen_ai_parent_span_id")
                .and_then(Value::as_str)
        })?;

    Some((trace_id.to_string(), parent_span_id.to_string()))
}

pub(super) fn current_otel_trace_id(span: &Span) -> Option<String> {
    let span_context = span.context().span().span_context().clone();
    if span_context.is_valid() {
        Some(span_context.trace_id().to_string())
    } else {
        None
    }
}

pub(super) fn current_otel_span_id(span: &Span) -> Option<String> {
    let span_context = span.context().span().span_context().clone();
    if span_context.is_valid() {
        Some(span_context.span_id().to_string())
    } else {
        None
    }
}

pub(in crate::state::dispatch) fn record_wasm_error_on_current_span(error: &str) {
    let span = Span::current();
    record_wasm_error_on_span(&span, error);
}

pub(super) fn record_wasm_error_on_span(span: &Span, error: &str) {
    let error_type = integration_error_type(error);
    span.record("error.type", error_type.as_str());
    span.record("error.message", error);
    span.record("exception.message", error);
    span.set_status(Status::error(error.to_string()));
}

pub(super) fn build_llm_root_span(
    ctx: &WasmDispatchCtx<'_>,
    integration: &temper_spec::automaton::Integration,
    entity_state: &EntityState,
    module_name: &str,
) -> Span {
    let provider = entity_state
        .fields
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("anthropic");
    let provider = normalize_llm_provider_for_observability(provider);
    let model = entity_state
        .fields
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let session_id = ctx
        .agent_ctx
        .session_id
        .as_deref()
        .unwrap_or(ctx.entity_ref.entity_id);

    let span = tracing::info_span!(
        "llm_caller.trace",
        otel.name = %format!("wasm:{module_name}"),
        integration = %integration.name,
        wasm.module = %module_name,
        gen_ai.system = %provider,
        gen_ai.provider.name = %provider,
        gen_ai.system_instructions = tracing::field::Empty,
        gen_ai.request.model = %model,
        gen_ai.operation.name = "chat",
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.conversation.id = %session_id,
        gen_ai.input.messages = tracing::field::Empty,
        gen_ai.output.messages = tracing::field::Empty,
        error.type = tracing::field::Empty,
        error.message = tracing::field::Empty,
        exception.message = tracing::field::Empty,
    );
    span
}

pub(super) fn attach_llm_parent_context(
    span: &Span,
    llm_parent_span_id: Option<&str>,
    entity_state: &EntityState,
    session_id: &str,
    duration_ms: u64,
    callback_params: &mut Value,
) {
    let span_context = span.context().span().span_context().clone();
    if !span_context.is_valid() {
        return;
    }

    let Some(object) = callback_params.as_object_mut() else {
        return;
    };

    object.insert(
        "_gen_ai_parent_trace_id".into(),
        json!(span_context.trace_id().to_string()),
    );
    object.insert(
        "gen_ai_parent_trace_id".into(),
        json!(span_context.trace_id().to_string()),
    );
    object.insert(
        "_gen_ai_parent_span_id".into(),
        json!(span_context.span_id().to_string()),
    );
    object.insert(
        "gen_ai_parent_span_id".into(),
        json!(span_context.span_id().to_string()),
    );
    if let Some(parent_span_id) = llm_parent_span_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        object.insert(
            "_gen_ai_llm_parent_span_id".into(),
            json!(parent_span_id.to_string()),
        );
        object.insert(
            "gen_ai_llm_parent_span_id".into(),
            json!(parent_span_id.to_string()),
        );
    }

    let trace_id = span_context.trace_id().to_string();
    let agent_span_id = entity_state
        .fields
        .get("llmobs_agent_span_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            temper_observe::llmobs_api::derive_span_id(&format!("{trace_id}:{session_id}:agent"))
        });
    object.insert(
        "_gen_ai_llmobs_agent_span_id".into(),
        json!(agent_span_id.clone()),
    );
    object.insert("llmobs_agent_span_id".into(), json!(agent_span_id));

    let agent_start_ns = entity_state
        .fields
        .get("llmobs_agent_start_ns")
        .and_then(value_as_u64)
        .unwrap_or_else(|| llmobs_agent_start_ns_for_duration(duration_ms));
    object.insert(
        "_gen_ai_llmobs_agent_start_ns".into(),
        json!(agent_start_ns),
    );
    object.insert("llmobs_agent_start_ns".into(), json!(agent_start_ns));

    let workflow_span_id = temper_observe::llmobs_api::derive_span_id(&format!(
        "{}:{}:workflow",
        span_context.trace_id(),
        span_context.span_id()
    ));
    object.insert(
        "_gen_ai_llmobs_workflow_span_id".into(),
        json!(workflow_span_id.clone()),
    );
    object.insert("llmobs_workflow_span_id".into(), json!(workflow_span_id));
}

fn llmobs_agent_start_ns(entity_state: &EntityState, callback_params: &Value) -> Option<u64> {
    callback_params
        .get("_gen_ai_llmobs_agent_start_ns")
        .and_then(value_as_u64)
        .or_else(|| {
            callback_params
                .get("llmobs_agent_start_ns")
                .and_then(value_as_u64)
        })
        .or_else(|| {
            entity_state
                .fields
                .get("llmobs_agent_start_ns")
                .and_then(value_as_u64)
        })
}

fn llmobs_agent_start_ns_for_duration(duration_ms: u64) -> u64 {
    current_unix_ns().saturating_sub(duration_ms.saturating_add(100).saturating_mul(1_000_000))
}

fn current_unix_ns() -> u64 {
    SystemTime::now() // determinism-ok: LLM observability timestamp translation
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

pub(super) fn strip_private_observability_params(mut params: Value) -> Value {
    let Some(object) = params.as_object_mut() else {
        return params;
    };

    object.retain(|key, _| !key.starts_with("_gen_ai_") && key.as_str() != "_dd_llmobs_tool_spans");
    params
}

fn integration_error_type(error: &str) -> String {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("rate limit") {
        "rate_limit".to_string()
    } else if normalized.contains("timeout") {
        "timeout".to_string()
    } else if normalized.contains("authorization denied") {
        "authorization_denied".to_string()
    } else if normalized.contains("connection") {
        "connection_error".to_string()
    } else {
        "integration_error".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{WasmDispatchMode, WasmEntityRef};
    use super::*;
    use crate::request_context::AgentContext;
    use temper_runtime::tenant::TenantId;

    #[test]
    fn strips_private_llm_observability_params_before_callback_dispatch() {
        let params = json!({
            "provider_response_file_id": "file-123",
            "input_tokens": 10,
            "_gen_ai_input_messages": "[{\"role\":\"user\"}]",
            "_gen_ai_output_messages": "[{\"role\":\"assistant\"}]",
            "_gen_ai_system_instructions": "system",
            "_gen_ai_provider": "anthropic",
            "_gen_ai_model": "claude-sonnet-4-6",
            "_gen_ai_finish_reason": "end_turn",
            "_gen_ai_llm_parent_span_id": "parent-span-private",
            "_dd_llmobs_tool_spans": "[]",
            "gen_ai_parent_trace_id": "trace-public",
            "gen_ai_llm_parent_span_id": "parent-span-public",
        });

        let stripped = strip_private_observability_params(params);

        assert_eq!(stripped["provider_response_file_id"], "file-123");
        assert_eq!(stripped["input_tokens"], 10);
        assert_eq!(stripped["gen_ai_parent_trace_id"], "trace-public");
        assert_eq!(stripped["gen_ai_llm_parent_span_id"], "parent-span-public");
        assert!(stripped.get("_gen_ai_input_messages").is_none());
        assert!(stripped.get("_gen_ai_output_messages").is_none());
        assert!(stripped.get("_gen_ai_system_instructions").is_none());
        assert!(stripped.get("_gen_ai_provider").is_none());
        assert!(stripped.get("_gen_ai_model").is_none());
        assert!(stripped.get("_gen_ai_finish_reason").is_none());
        assert!(stripped.get("_gen_ai_llm_parent_span_id").is_none());
        assert!(stripped.get("_dd_llmobs_tool_spans").is_none());
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
            sequence_nr: 0,
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
            sequence_nr: 0,
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
        let llm_trace_id = parent.in_scope(|| {
            let ctx = WasmDispatchCtx {
                entity_ref: WasmEntityRef {
                    tenant: &tenant,
                    entity_type: "Session",
                    entity_id: "ss-1",
                },
                action: "ContextReady",
                agent_ctx: &agent_ctx,
                mode: WasmDispatchMode::Inline,
            };
            let span = build_llm_root_span(&ctx, &integration, &entity_state, "provider_caller");
            span.context().span().span_context().trace_id().to_string()
        });

        assert_eq!(llm_trace_id, expected_trace_id);
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
            sequence_nr: 0,
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
        let expected_agent_span_id = temper_observe::llmobs_api::derive_span_id(&format!(
            "{expected_trace_id}:session-1:agent"
        ));
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
            sequence_nr: 0,
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
            sequence_nr: 0,
        };

        assert_eq!(
            llmobs_tool_trace_and_parent(&entity_state, &json!({})),
            Some(("trace-1".to_string(), "workflow-parent".to_string()))
        );
    }
}
