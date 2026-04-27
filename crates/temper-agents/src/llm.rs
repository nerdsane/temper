//! LLM integration actor — calls AI Gateway and uses Bus for token streaming.
//!
//! No transport dependency: tokens are sent via `ctx.tell(Bus, StreamMsg)`.
//! The BusActor implementation (e.g. ZenohBusActor) owns the transport details.
//!
//! Auth: AI_GATEWAY_TOKEN env var.

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use temper_actor_runtime::bus::{BUS_ACTOR_TYPE, StreamMsg};
use temper_actor_runtime::spec_actor::SpecMessage;
use temper_actor_runtime::{Actor, ActorContext, ActorError, ActorHandle, Message};

use crate::common::{decode_params, message_action, session_id_from_namespace};

const AI_GATEWAY_URL: &str = "https://ai-gateway.us1.staging.dog/v1/chat/completions";
const MODEL: &str = "claude-haiku-4-5";
const SYSTEM_PROMPT: &str = "You are a helpful assistant with access to a bash tool. \
Use it to run shell commands when needed. Keep responses concise.";

// ─── OpenAI-compatible types ──────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatMessage {
    pub role: String,
    pub content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    index: Option<u32>,
    id: Option<String>,
    function: Option<StreamFunction>,
}

#[derive(Deserialize)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

// ─── Message builders ─────────────────────────────────────────────────────────

pub fn user_msg(content: impl Into<Value>) -> ChatMessage {
    ChatMessage {
        role: "user".into(),
        content: content.into(),
        tool_calls: None,
        tool_call_id: None,
    }
}

pub fn assistant_msg(content: impl Into<Value>) -> ChatMessage {
    ChatMessage {
        role: "assistant".into(),
        content: content.into(),
        tool_calls: None,
        tool_call_id: None,
    }
}

pub fn assistant_tool_call_msg(tool_calls: Vec<Value>) -> ChatMessage {
    ChatMessage {
        role: "assistant".into(),
        content: Value::Null,
        tool_calls: Some(tool_calls),
        tool_call_id: None,
    }
}

pub fn tool_result_msg(id: String, content: String) -> ChatMessage {
    ChatMessage {
        role: "tool".into(),
        content: json!(content),
        tool_calls: None,
        tool_call_id: Some(id),
    }
}

// ─── Actor ────────────────────────────────────────────────────────────────────

pub struct LlmIntegrationActor {
    pub http: reqwest::Client,
}

impl LlmIntegrationActor {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Actor for LlmIntegrationActor {
    fn actor_type(&self) -> &str {
        "LlmIntegration"
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        if message_action(message) != "invoke_llm" {
            return Ok(());
        }

        let from = match &message.from {
            Some(f) => f.clone(),
            None => return Ok(()),
        };

        let params = decode_params(message);
        let session_id = session_id_from_namespace(&ctx.self_handle().namespace).to_string();

        // Load conversation history from state.
        let mut history: Vec<ChatMessage> = if state.is_empty() {
            vec![]
        } else {
            serde_json::from_slice(state).unwrap_or_default()
        };

        // Add new turn.
        if let Some(prompt) = params["user_prompt"].as_str() {
            history.push(user_msg(json!(prompt)));
        } else if let Some(results) = params["tool_results"].as_array() {
            for r in results {
                history.push(tool_result_msg(
                    r["tool_use_id"].as_str().unwrap_or("").to_string(),
                    r["content"].as_str().unwrap_or("").to_string(),
                ));
            }
        } else if history.is_empty() {
            ctx.tell(&from, SpecMessage::new("InferenceCompleteEndTurn"))
                .await;
            return Ok(());
        }

        let token = std::env::var("AI_GATEWAY_TOKEN").unwrap_or_default();
        let namespace = ctx.self_handle().namespace.clone();

        // Build tool list dynamically from ToolRegistry.
        let registry = ActorHandle::new(namespace.clone(), "ToolRegistry");
        let tools = if let Ok(resp) = ctx
            .ask(
                &registry,
                SpecMessage::new("GetAllTools"),
                Duration::from_secs(3),
            )
            .await
        {
            let reg_params = resp
                .decode::<SpecMessage>()
                .ok()
                .and_then(|m| serde_json::from_slice::<serde_json::Value>(&m.params).ok())
                .unwrap_or(json!({}));
            reg_params["tools"].as_array()
                .map(|all| all.iter().map(|t| {
                    let name = t["name"].as_str().unwrap_or("unknown");
                    let description = t["description"].as_str().unwrap_or("Tool");
                    let parameters = if t["json_schema"].is_object() && !t["json_schema"].is_null() {
                        t["json_schema"].clone()
                    } else {
                        json!({ "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] })
                    };
                    json!({ "type": "function", "function": { "name": name, "description": description, "parameters": parameters } })
                }).collect::<Vec<_>>())
                .unwrap_or_default()
        } else {
            vec![
                json!({ "type": "function", "function": { "name": "bash", "description": "Run a bash command.", "parameters": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] } } }),
            ]
        };

        tracing::info!("[LLM] calling with {} tool(s)", tools.len());

        let body = json!({
            "model": MODEL,
            "max_tokens": 1024,
            "stream": true,
            "system": SYSTEM_PROMPT,
            "messages": history,
            "tools": tools,
        });

        let response = self
            .http
            .post(AI_GATEWAY_URL)
            .header("Authorization", format!("Bearer {token}"))
            .header("source", "bits-actor-demo")
            .header("org-id", "2")
            .header("provider", "anthropic")
            .json(&body)
            .send()
            .await
            .map_err(|e| ActorError::HandlerFailed(format!("LLM request: {e}")))?;

        if !response.status().is_success() {
            let err = response.text().await.unwrap_or_default();
            ctx.tell(
                &from,
                SpecMessage::with_params("InferenceFailed", json!({ "error": err })),
            )
            .await;
            return Ok(());
        }

        // Parse SSE stream.
        let mut byte_stream = response.bytes_stream();
        let mut full_text = String::new();
        let mut finish_reason = String::new();
        let mut tool_call_accum: std::collections::HashMap<u32, (String, String, String)> =
            Default::default();
        let mut buffer = String::new();

        while let Some(chunk) = byte_stream.next().await {
            let bytes = chunk.map_err(|e| ActorError::HandlerFailed(format!("stream: {e}")))?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim().to_string();
                buffer = buffer[pos + 1..].to_string();
                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    break;
                }
                let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
                    continue;
                };
                let Some(choice) = chunk.choices.into_iter().next() else {
                    continue;
                };
                if let Some(r) = choice.finish_reason
                    && !r.is_empty()
                {
                    finish_reason = r;
                }
                if let Some(text) = choice.delta.content {
                    full_text.push_str(&text);
                }
                if let Some(tcs) = choice.delta.tool_calls {
                    for tc in tcs {
                        let idx = tc.index.unwrap_or(0);
                        let e = tool_call_accum.entry(idx).or_insert_with(|| {
                            (
                                tc.id.clone().unwrap_or_default(),
                                String::new(),
                                String::new(),
                            )
                        });
                        if let Some(f) = &tc.function {
                            if let Some(n) = &f.name {
                                e.1.push_str(n);
                            }
                            if let Some(a) = &f.arguments {
                                e.2.push_str(a);
                            }
                        }
                        if tc.id.is_some() {
                            e.0 = tc.id.unwrap_or_default();
                        }
                    }
                }
            }
        }

        if finish_reason == "tool_calls" || !tool_call_accum.is_empty() {
            let mut sorted: Vec<_> = tool_call_accum.into_iter().collect();
            sorted.sort_by_key(|(i, _)| *i);

            let tc_values: Vec<Value> = sorted
                .iter()
                .map(|(_, (id, name, args))| {
                    json!({
                        "id": id, "type": "function",
                        "function": { "name": name, "arguments": args }
                    })
                })
                .collect();

            history.push(assistant_tool_call_msg(tc_values));

            let calls: Vec<Value> = sorted
                .iter()
                .map(|(_, (id, name, args))| {
                    let input: Value = serde_json::from_str(args).unwrap_or(json!({}));
                    json!({ "id": id, "name": name, "input": input })
                })
                .collect();

            *state = serde_json::to_vec(&history).unwrap_or_default();
            ctx.tell(
                &from,
                SpecMessage::with_params(
                    "InferenceCompleteToolCalls",
                    json!({ "tool_calls": calls }),
                ),
            )
            .await;
        } else {
            tracing::info!("[LLM] response ({} chars)", full_text.len());
            history.push(assistant_msg(json!(full_text.clone())));
            *state = serde_json::to_vec(&history).unwrap_or_default();

            // Stream response to client via Bus actor (transport-agnostic).
            let bus = ActorHandle::new(ctx.self_handle().namespace.clone(), BUS_ACTOR_TYPE);
            ctx.tell(
                &bus,
                StreamMsg {
                    session_id: session_id.clone(),
                    payload: full_text.as_bytes().to_vec(),
                },
            )
            .await;

            ctx.tell(
                &from,
                SpecMessage::with_params(
                    "InferenceCompleteEndTurn",
                    json!({ "response": full_text }),
                ),
            )
            .await;
        }

        Ok(())
    }
}
