use std::{
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::Client;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
#[path = "llmobs_endpoint.rs"]
mod llmobs_endpoint;
#[path = "llmobs_format.rs"]
mod llmobs_format;
use llmobs_endpoint::llmobs_endpoint;
pub use llmobs_format::parse_tool_span_inputs;
use llmobs_format::{convert_otel_messages_to_llmobs, parent_io_value_from_messages};

#[derive(Clone, Debug)]
struct LlmObsConfig {
    api_key: String,
    endpoint: String,
}

#[derive(Clone, Debug)]
pub struct LlmSpanInput<'a> {
    pub service_name: &'a str,
    pub session_id: &'a str,
    pub trace_id: &'a str,
    pub span_id: &'a str,
    pub parent_span_id: Option<&'a str>,
    pub agent_span_id: Option<&'a str>,
    pub agent_start_ns: Option<u64>,
    pub workflow_span_id: Option<&'a str>,
    pub agent_name: Option<&'a str>,
    pub workflow_name: Option<&'a str>,
    pub span_name: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub system_instructions: Option<&'a str>,
    pub input_messages_json: Option<&'a str>,
    pub output_messages_json: Option<&'a str>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub finish_reason: Option<&'a str>,
    pub duration_ms: u64,
    pub error_type: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub struct ToolSpanInput<'a> {
    pub service_name: &'a str,
    pub session_id: &'a str,
    pub trace_id: &'a str,
    pub parent_span_id: &'a str,
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
    pub arguments_json: &'a str,
    pub result_text: &'a str,
    pub duration_ms: u64,
    pub is_error: bool,
}

static LLMOBS_CONFIG: OnceLock<Option<LlmObsConfig>> = OnceLock::new();
static LLMOBS_CLIENT: OnceLock<Client> = OnceLock::new();
pub async fn submit_llm_span(input: LlmSpanInput<'_>) -> Result<(), String> {
    let Some(config) = llmobs_config().as_ref().cloned() else {
        return Ok(());
    };

    let payload = build_llm_span_payload(&input)?;
    post_payload(&config, payload).await
}
fn build_llm_span_payload(input: &LlmSpanInput<'_>) -> Result<Value, String> {
    let mut input_messages = input
        .input_messages_json
        .map(convert_otel_messages_to_llmobs)
        .transpose()
        .map_err(|error| format!("failed to convert input messages: {error}"))?
        .unwrap_or_default();
    if let Some(system) = input
        .system_instructions
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        input_messages.insert(0, json!({ "role": "system", "content": system }));
    }

    let output_messages = input
        .output_messages_json
        .map(convert_otel_messages_to_llmobs)
        .transpose()
        .map_err(|error| format!("failed to convert output messages: {error}"))?
        .unwrap_or_default();
    let parent_input_value = parent_io_value_from_messages(&input_messages, "user");
    let parent_output_value = parent_io_value_from_messages(&output_messages, "assistant");

    let start_ns = approximate_start_ns(input.duration_ms);
    let duration_ns_u64 = input.duration_ms.saturating_mul(1_000_000);
    let duration_ns = duration_ns_u64 as f64;
    let end_ns = start_ns.saturating_add(duration_ns_u64);
    let trace_id = normalize_trace_id(input.trace_id);
    let span_id = normalize_span_id(input.span_id);
    let apm_parent_span_id = input
        .parent_span_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_span_id);
    let agent_span_id = input
        .agent_span_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_span_id);
    let workflow_span_id = input
        .workflow_span_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_span_id);
    let llm_parent_span_id = workflow_span_id
        .as_deref()
        .or(apm_parent_span_id.as_deref())
        .unwrap_or("undefined")
        .to_string();
    let provider = normalize_model_provider(input.provider);
    let mut metadata = Map::from_iter([
        ("model_name".to_string(), json!(input.model)),
        ("model_provider".to_string(), json!(provider)),
    ]);

    let mut meta = Map::from_iter([
        ("kind".to_string(), json!("llm")),
        ("model_name".to_string(), json!(input.model)),
        ("model_provider".to_string(), json!(provider)),
    ]);
    if !input_messages.is_empty() {
        meta.insert("input".to_string(), json!({ "messages": input_messages }));
    }
    if !output_messages.is_empty() {
        meta.insert("output".to_string(), json!({ "messages": output_messages }));
    }
    if input.finish_reason.is_some() || input.error_type.is_some() {
        if let Some(finish_reason) = input.finish_reason.filter(|value| !value.is_empty()) {
            metadata.insert("finish_reason".to_string(), json!(finish_reason));
        }
        if let Some(error_type) = input.error_type.filter(|value| !value.is_empty()) {
            metadata.insert("error_type".to_string(), json!(error_type));
        }
    }
    meta.insert("metadata".to_string(), Value::Object(metadata));
    if let Some(error_type) = input.error_type.filter(|value| !value.is_empty()) {
        meta.insert(
            "error".to_string(),
            json!({
                "message": error_type,
                "type": error_type,
            }),
        );
    }

    let mut metrics = Map::new();
    if input.input_tokens > 0 {
        metrics.insert("input_tokens".to_string(), json!(input.input_tokens as f64));
    }
    if input.output_tokens > 0 {
        metrics.insert(
            "output_tokens".to_string(),
            json!(input.output_tokens as f64),
        );
    }
    let total_tokens = input.input_tokens.saturating_add(input.output_tokens);
    if total_tokens > 0 {
        metrics.insert("total_tokens".to_string(), json!(total_tokens as f64));
    }

    let span_tags = vec![
        format!("service:{}", input.service_name),
        format!("session_id:{}", input.session_id),
        format!("model_name:{}", input.model),
        format!("model_provider:{provider}"),
    ];

    let status = if input.error_type.is_some() {
        "error"
    } else {
        "ok"
    };
    let mut spans = Vec::new();

    if let Some(agent_span_id) = agent_span_id.as_deref() {
        let agent_start_ns = input
            .agent_start_ns
            .unwrap_or_else(|| start_ns.saturating_sub(100_000_000))
            .min(start_ns.saturating_sub(100_000_000));
        let agent_duration_ns = end_ns
            .saturating_add(100_000_000)
            .saturating_sub(agent_start_ns)
            .max(duration_ns_u64.saturating_add(200_000_000));
        let agent_meta = parent_agent_or_workflow_meta(
            "agent",
            input.session_id,
            parent_input_value.as_deref(),
            parent_output_value.as_deref(),
        );
        spans.push(json!({
            "parent_id": "undefined",
            "trace_id": trace_id,
            "span_id": agent_span_id,
            "name": input.agent_name.unwrap_or("temperpaw.agent.session"),
            "service": input.service_name,
            "ml_app": input.service_name,
            "session_id": input.session_id,
            "tags": span_tags.clone(),
            "status": status,
            "start_ns": agent_start_ns,
            "duration": agent_duration_ns as f64,
            "meta": agent_meta,
        }));
    }

    if let Some(workflow_span_id) = workflow_span_id.as_deref() {
        let workflow_start_ns = start_ns.saturating_sub(50_000_000);
        let workflow_parent_id = agent_span_id
            .as_deref()
            .or(apm_parent_span_id.as_deref())
            .unwrap_or("undefined");
        let workflow_meta = parent_agent_or_workflow_meta(
            "workflow",
            input.session_id,
            parent_input_value.as_deref(),
            parent_output_value.as_deref(),
        );
        spans.push(json!({
            "parent_id": workflow_parent_id,
            "trace_id": trace_id,
            "span_id": workflow_span_id,
            "name": input.workflow_name.unwrap_or("temperpaw.agent_turn"),
            "service": input.service_name,
            "ml_app": input.service_name,
            "session_id": input.session_id,
            "tags": span_tags.clone(),
            "status": status,
            "start_ns": workflow_start_ns,
            "duration": duration_ns + 100_000_000.0,
            "meta": workflow_meta,
        }));
    }

    spans.push(json!({
        "parent_id": llm_parent_span_id,
        "trace_id": trace_id,
        "span_id": span_id,
        "name": input.span_name,
        "service": input.service_name,
        "ml_app": input.service_name,
        "session_id": input.session_id,
        "tags": span_tags,
        "status": status,
        "start_ns": start_ns,
        "duration": duration_ns,
        "meta": Value::Object(meta),
        "metrics": Value::Object(metrics),
    }));

    Ok(json!({
        "data": {
            "type": "span",
            "attributes": {
                "ml_app": input.service_name,
                "session_id": input.session_id,
                "tags": [format!("service:{}", input.service_name), format!("session_id:{}", input.session_id)],
                "spans": spans,
            },
        },
    }))
}

fn parent_agent_or_workflow_meta(
    kind: &str,
    session_id: &str,
    input_value: Option<&str>,
    output_value: Option<&str>,
) -> Value {
    let mut meta = Map::from_iter([
        ("kind".to_string(), json!(kind)),
        (
            "metadata".to_string(),
            json!({
                "session_id": session_id,
            }),
        ),
    ]);
    if let Some(value) = input_value {
        meta.insert("input".to_string(), json!({ "value": value }));
    }
    if let Some(value) = output_value {
        meta.insert("output".to_string(), json!({ "value": value }));
    }
    Value::Object(meta)
}

pub async fn submit_tool_spans(
    service_name: &str,
    session_id: &str,
    trace_id: &str,
    parent_span_id: &str,
    spans: &[ToolSpanInput<'_>],
) -> Result<(), String> {
    let Some(config) = llmobs_config().as_ref().cloned() else {
        return Ok(());
    };
    if spans.is_empty() {
        return Ok(());
    }

    let trace_id = normalize_trace_id(trace_id);
    let parent_span_id = normalize_span_id(parent_span_id);
    let mut rendered_spans = Vec::with_capacity(spans.len());
    let mut start_cursor_ns = approximate_start_ns(
        spans
            .iter()
            .map(|span| span.duration_ms)
            .sum::<u64>()
            .saturating_add(50),
    );

    for span in spans {
        let duration_ns = (span.duration_ms as f64) * 1_000_000.0;
        let tool_span_id = hash_to_decimal_id(&format!(
            "{}:{}:{}",
            trace_id, parent_span_id, span.tool_call_id
        ));
        let mut meta = Map::from_iter([
            ("kind".to_string(), json!("tool")),
            (
                "input".to_string(),
                json!({
                    "value": span.arguments_json,
                }),
            ),
            (
                "output".to_string(),
                json!({
                    "value": span.result_text,
                }),
            ),
            (
                "metadata".to_string(),
                json!({
                    "tool_call_id": span.tool_call_id,
                }),
            ),
        ]);
        if span.is_error {
            meta.insert(
                "error".to_string(),
                json!({
                    "message": span.result_text,
                    "type": "tool_call_error",
                }),
            );
        }
        rendered_spans.push(json!({
            "parent_id": parent_span_id,
            "trace_id": trace_id,
            "span_id": tool_span_id,
            "name": span.tool_name,
            "service": service_name,
            "ml_app": service_name,
            "session_id": session_id,
            "status": if span.is_error { "error" } else { "ok" },
            "start_ns": start_cursor_ns,
            "duration": duration_ns,
            "meta": Value::Object(meta),
        }));
        start_cursor_ns = start_cursor_ns.saturating_add(span.duration_ms * 1_000_000 + 1);
    }

    let payload = json!({
        "data": {
            "type": "span",
            "attributes": {
                "ml_app": service_name,
                "session_id": session_id,
                "tags": [format!("service:{}", service_name)],
                "spans": rendered_spans,
            },
        },
    });

    post_payload(&config, payload).await
}

fn llmobs_config() -> &'static Option<LlmObsConfig> {
    LLMOBS_CONFIG.get_or_init(|| {
        let api_key = read_non_empty_env("DD_API_KEY")?;

        let enabled = match read_non_empty_env("DD_LLMOBS_API_ENABLED") {
            Some(value) => !matches!(value.as_str(), "0" | "false" | "False" | "FALSE"),
            None => {
                if read_non_empty_env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_some() {
                    false
                } else {
                    !read_non_empty_env("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
                        .map(|headers| headers.contains("dd-otlp-source=llmobs"))
                        .unwrap_or(false)
                }
            }
        };
        if !enabled {
            return None;
        }

        let site = read_non_empty_env("DD_SITE").unwrap_or_else(|| "datadoghq.com".to_string());
        let endpoint = llmobs_endpoint(&site, read_non_empty_env("DD_LLMOBS_ENDPOINT"));
        Some(LlmObsConfig { api_key, endpoint })
    })
}

fn llmobs_client() -> &'static Client {
    LLMOBS_CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build Datadog LLMObs client")
    })
}

async fn post_payload(config: &LlmObsConfig, payload: Value) -> Result<(), String> {
    let response = llmobs_client()
        .post(config.endpoint.as_str())
        .header("DD-API-KEY", &config.api_key)
        .json(&payload)
        .send()
        .await
        .map_err(|error| format!("LLMObs request failed: {error}"))?;

    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable response body>".to_string());
    Err(format!("LLMObs request failed ({status}): {body}"))
}

fn read_non_empty_env(var_name: &str) -> Option<String> {
    let value = std::env::var(var_name).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn approximate_start_ns(duration_ms: u64) -> u64 {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    now_ns.saturating_sub(duration_ms.saturating_mul(1_000_000))
}

fn normalize_trace_id(trace_id: &str) -> String {
    match u128::from_str_radix(trace_id.trim(), 16) {
        Ok(value) => value.to_string(),
        Err(_) => trace_id.trim().to_string(),
    }
}

fn normalize_span_id(span_id: &str) -> String {
    match u64::from_str_radix(span_id.trim(), 16) {
        Ok(value) => value.to_string(),
        Err(_) => span_id.trim().to_string(),
    }
}

fn normalize_model_provider(provider: &str) -> String {
    let trimmed = provider.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "openai_codex" => "openai".to_string(),
        "mock" => "custom".to_string(),
        "" => "custom".to_string(),
        _ => trimmed.to_string(),
    }
}

fn hash_to_decimal_id(seed: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(seed.as_bytes());
    let bytes = digest.finalize();
    let mut buffer = [0u8; 8];
    buffer.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(buffer).to_string()
}

pub fn derive_span_id(seed: &str) -> String {
    hash_to_decimal_id(seed)
}

#[cfg(test)]
#[path = "llmobs_api_test.rs"]
mod tests;
