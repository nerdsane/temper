use std::collections::BTreeMap;

use crate::wide_event::{
    EventKind, WideEvent, duration_ms, event_timestamp, new_span_id, otel::classify_error,
};

/// Input for building an LLM call wide event.
pub struct LlmCallInput<'a> {
    /// LLM provider (e.g., "anthropic", "openrouter", "openai").
    pub provider: &'a str,
    /// Model name (e.g., "claude-sonnet-4-6").
    pub model: &'a str,
    /// Operation name (e.g., "chat").
    pub operation: &'a str,
    /// Entity type (typically "Session").
    pub entity_type: &'a str,
    /// Entity ID (session ID).
    pub entity_id: &'a str,
    /// Session/conversation ID for grouping turns.
    pub session_id: &'a str,
    /// Whether the call succeeded.
    pub success: bool,
    /// Duration in nanoseconds.
    pub duration_ns: u64,
    /// Input tokens consumed.
    pub input_tokens: i64,
    /// Output tokens generated.
    pub output_tokens: i64,
    /// Stop reason (e.g., "end_turn", "tool_use").
    pub stop_reason: &'a str,
    /// System instructions passed separately from chat history.
    pub system_instructions: Option<&'a str>,
    /// Input messages serialized as a JSON array string.
    pub input_messages: Option<&'a str>,
    /// Output messages serialized as a JSON array string.
    pub output_messages: Option<&'a str>,
    /// Trace ID for correlation.
    pub trace_id: &'a str,
    /// Error message, if any.
    pub error: Option<&'a str>,
}

/// Build a WideEvent from an LLM API call.
pub fn from_llm_call(input: LlmCallInput<'_>) -> WideEvent {
    let span_id = new_span_id();
    let mut tags = BTreeMap::new();
    tags.insert("gen_ai.system".into(), input.provider.into());
    tags.insert("gen_ai.request.model".into(), input.model.into());
    tags.insert("gen_ai.operation.name".into(), input.operation.into());
    tags.insert(
        "gen_ai.response.finish_reasons".into(),
        input.stop_reason.into(),
    );
    tags.insert("success".into(), input.success.to_string());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(input.entity_id));
    attributes.insert(
        "gen_ai.conversation.id".into(),
        serde_json::json!(input.session_id),
    );
    if let Some(system_instructions) = input.system_instructions {
        attributes.insert(
            "gen_ai.system_instructions".into(),
            serde_json::json!(system_instructions),
        );
    }
    if let Some(messages) = input.input_messages {
        attributes.insert("gen_ai.input.messages".into(), serde_json::json!(messages));
    }
    if let Some(messages) = input.output_messages {
        attributes.insert("gen_ai.output.messages".into(), serde_json::json!(messages));
    }
    if let Some(err) = input.error {
        attributes.insert("error".into(), serde_json::json!(err));
        attributes.insert("error.message".into(), serde_json::json!(err));
        attributes.insert(
            "error.type".into(),
            serde_json::json!(classify_error(EventKind::LlmCall, err)),
        );
        attributes.insert("exception.message".into(), serde_json::json!(err));
    }

    let mut measurements = BTreeMap::new();
    measurements.insert(
        "gen_ai.usage.input_tokens".into(),
        input.input_tokens as f64,
    );
    measurements.insert(
        "gen_ai.usage.output_tokens".into(),
        input.output_tokens as f64,
    );
    measurements.insert("duration_ms".into(), duration_ms(input.duration_ns));
    measurements.insert("invocation_count".into(), 1.0);

    WideEvent {
        event_kind: EventKind::LlmCall,
        entity_type: input.entity_type.into(),
        entity_id: input.entity_id.into(),
        operation: input.operation.into(),
        from_status: String::new(),
        to_status: input.stop_reason.into(),
        success: input.success,
        duration_ns: input.duration_ns,
        timestamp: event_timestamp(),
        trace_id: input.trace_id.into(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

/// Input for building a tool call wide event.
pub struct ToolCallInput<'a> {
    /// Tool name (e.g., "temper_create", "sandbox_bash").
    pub tool_name: &'a str,
    /// Tool call identifier from the LLM response.
    pub tool_call_id: Option<&'a str>,
    /// Entity type (typically "Session").
    pub entity_type: &'a str,
    /// Entity ID (session ID).
    pub entity_id: &'a str,
    /// Session/conversation ID for grouping.
    pub session_id: &'a str,
    /// Tool arguments serialized as JSON.
    pub tool_arguments: Option<&'a str>,
    /// Tool result content serialized as JSON or text.
    pub tool_result: Option<&'a str>,
    /// Whether the tool call succeeded.
    pub success: bool,
    /// Duration in nanoseconds.
    pub duration_ns: u64,
    /// Trace ID for correlation.
    pub trace_id: &'a str,
    /// Error message, if any.
    pub error: Option<&'a str>,
}

/// Build a WideEvent from an agent tool invocation.
pub fn from_tool_call(input: ToolCallInput<'_>) -> WideEvent {
    let span_id = new_span_id();
    let mut tags = BTreeMap::new();
    tags.insert("gen_ai.operation.name".into(), "execute_tool".into());
    tags.insert("gen_ai.tool.name".into(), input.tool_name.into());
    tags.insert("entity_type".into(), input.entity_type.into());
    tags.insert("success".into(), input.success.to_string());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(input.entity_id));
    attributes.insert(
        "gen_ai.conversation.id".into(),
        serde_json::json!(input.session_id),
    );
    if let Some(tool_call_id) = input.tool_call_id {
        attributes.insert(
            "gen_ai.tool.call.id".into(),
            serde_json::json!(tool_call_id),
        );
    }
    if let Some(arguments) = input.tool_arguments {
        attributes.insert(
            "gen_ai.tool.call.arguments".into(),
            serde_json::json!(arguments),
        );
    }
    if let Some(result) = input.tool_result {
        attributes.insert("gen_ai.tool.call.result".into(), serde_json::json!(result));
    }
    if let Some(err) = input.error {
        attributes.insert("error".into(), serde_json::json!(err));
        attributes.insert("error.message".into(), serde_json::json!(err));
        attributes.insert(
            "error.type".into(),
            serde_json::json!(classify_error(EventKind::ToolCall, err)),
        );
        attributes.insert("exception.message".into(), serde_json::json!(err));
    }

    let mut measurements = BTreeMap::new();
    measurements.insert("duration_ms".into(), duration_ms(input.duration_ns));
    measurements.insert("invocation_count".into(), 1.0);

    WideEvent {
        event_kind: EventKind::ToolCall,
        entity_type: input.entity_type.into(),
        entity_id: input.entity_id.into(),
        operation: "execute_tool".into(),
        from_status: String::new(),
        to_status: String::new(),
        success: input.success,
        duration_ns: input.duration_ns,
        timestamp: event_timestamp(),
        trace_id: input.trace_id.into(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}
