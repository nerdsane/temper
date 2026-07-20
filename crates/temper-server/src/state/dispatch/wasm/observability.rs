//! WASM and LLM observability payload helpers.

use super::*;

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

pub(super) fn normalize_llm_provider_for_observability(provider: &str) -> String {
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

pub(super) fn llmobs_tool_trace_and_parent(
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

pub(super) fn record_wasm_error_on_current_span(error: &str) {
    let span = Span::current();
    record_wasm_error_on_span(&span, error);
}
