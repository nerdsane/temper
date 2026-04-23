use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
struct LlmObsConfig {
    api_key: String,
    site: String,
}

#[derive(Clone, Debug)]
pub struct LlmSpanInput<'a> {
    pub service_name: &'a str,
    pub session_id: &'a str,
    pub trace_id: &'a str,
    pub span_id: &'a str,
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

    let start_ns = approximate_start_ns(input.duration_ms);
    let duration_ns = (input.duration_ms as f64) * 1_000_000.0;
    let trace_id = normalize_trace_id(input.trace_id);
    let span_id = normalize_span_id(input.span_id);
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

    Ok(json!({
        "data": {
            "type": "span",
            "attributes": {
                "ml_app": input.service_name,
                "session_id": input.session_id,
                "tags": span_tags.clone(),
                "spans": [{
                    "parent_id": "undefined",
                    "trace_id": trace_id,
                    "span_id": span_id,
                    "name": input.span_name,
                    "service": input.service_name,
                    "ml_app": input.service_name,
                    "session_id": input.session_id,
                    "tags": span_tags,
                    "status": if input.error_type.is_some() { "error" } else { "ok" },
                    "start_ns": start_ns,
                    "duration": duration_ns,
                    "meta": Value::Object(meta),
                    "metrics": Value::Object(metrics),
                }],
            },
        },
    }))
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
        Some(LlmObsConfig { api_key, site })
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
    let endpoint = format!(
        "https://api.{}/api/intake/llm-obs/v1/trace/spans",
        config.site.trim().trim_end_matches('/')
    );
    let response = llmobs_client()
        .post(endpoint)
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
    std::env::var(var_name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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

fn convert_otel_messages_to_llmobs(raw: &str) -> Result<Vec<Value>, serde_json::Error> {
    let parsed: Vec<Value> = serde_json::from_str(raw)?;
    Ok(parsed
        .into_iter()
        .filter_map(convert_otel_message)
        .collect())
}

fn convert_otel_message(message: Value) -> Option<Value> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant");
    let parts = message.get("parts").and_then(Value::as_array)?;

    let mut content_chunks = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();

    for part in parts {
        match part.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(content) = part.get("content").and_then(Value::as_str) {
                    content_chunks.push(content.to_string());
                }
            }
            "tool_call" => {
                let arguments = match part.get("arguments") {
                    Some(Value::Object(map)) => Value::Object(map.clone()),
                    Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({ "raw": raw })),
                    Some(other) => json!({ "raw": other.to_string() }),
                    None => json!({}),
                };
                tool_calls.push(json!({
                    "name": part.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                    "arguments": arguments,
                    "tool_id": part.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "type": "tool_call",
                }));
            }
            "tool_call_response" => {
                let result = match part.get("result") {
                    Some(Value::String(raw)) => raw.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                tool_results.push(json!({
                    "name": part.get("name").and_then(Value::as_str).unwrap_or("tool_result"),
                    "result": result,
                    "tool_id": part.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "type": "tool_result",
                }));
            }
            _ => {}
        }
    }

    let mut rendered = Map::from_iter([
        ("role".to_string(), json!(role)),
        ("content".to_string(), json!(content_chunks.join("\n"))),
    ]);
    if !tool_calls.is_empty() {
        rendered.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    if !tool_results.is_empty() {
        rendered.insert("tool_results".to_string(), Value::Array(tool_results));
    }
    Some(Value::Object(rendered))
}

pub fn parse_tool_span_inputs(
    raw_events: &[Value],
) -> Result<Vec<BTreeMap<String, Value>>, serde_json::Error> {
    raw_events
        .iter()
        .map(|value| match value {
            Value::Object(map) => Ok(map
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()),
            other => serde_json::from_value::<BTreeMap<String, Value>>(other.clone()),
        })
        .collect()
}

#[cfg(test)]
#[path = "llmobs_api_test.rs"]
mod tests;
