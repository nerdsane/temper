use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

pub(super) fn convert_otel_messages_to_llmobs(raw: &str) -> Result<Vec<Value>, serde_json::Error> {
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
