//! LLMObs span construction and context propagation.

use super::*;

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
        dd_llmobs_enabled = false,
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

pub(super) fn llmobs_agent_start_ns(
    entity_state: &EntityState,
    callback_params: &Value,
) -> Option<u64> {
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

pub(super) fn llmobs_agent_start_ns_for_duration(duration_ms: u64) -> u64 {
    current_unix_ns().saturating_sub(duration_ms.saturating_add(100).saturating_mul(1_000_000))
}

pub(super) fn current_unix_ns() -> u64 {
    SystemTime::now() // determinism-ok: LLM observability timestamp translation
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

pub(super) fn value_as_u64(value: &Value) -> Option<u64> {
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

pub(super) fn integration_error_type(error: &str) -> String {
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
