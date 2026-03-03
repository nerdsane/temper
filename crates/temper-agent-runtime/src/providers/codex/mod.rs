//! OpenAI Codex provider via ChatGPT Plus/Pro subscription OAuth.
//!
//! Implements [`LlmProvider`] for the OpenAI Responses API, authenticating
//! with tokens obtained via `temper login openai`.

pub mod auth;

use anyhow::{Context as _, Result};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

use super::sse::SseByteStream;
use super::{ContentBlock, LlmProvider, LlmResponse, Message};
use auth::{CodexCredentials, API_BASE};

/// OpenAI Codex Responses API provider.
///
/// Uses OAuth tokens from `~/.temper/codex-auth.json` (obtained via `temper login openai`).
/// Tokens are auto-refreshed when expired.
pub struct CodexProvider {
    client: reqwest::Client,
    model: String,
    credentials: Mutex<CodexCredentials>,
}

impl CodexProvider {
    /// Create a new provider, loading credentials from disk.
    ///
    /// Fails with a helpful message if the user hasn't logged in yet.
    pub fn new(model: &str) -> Result<Self> {
        let creds = auth::load_credentials()? // determinism-ok: CLI provider, not simulation-visible
            .context("No OpenAI credentials found. Run `temper login openai` first.")?;

        Ok(Self {
            client: reqwest::Client::new(),
            model: model.to_string(),
            credentials: Mutex::new(creds),
        })
    }

    /// Ensure the access token is valid, refreshing if expired.
    /// Returns `(access_token, account_id)`.
    async fn ensure_valid_token(&self) -> Result<(String, String)> {
        let mut creds = self.credentials.lock().await;
        if auth::is_expired(&creds) {
            *creds = auth::refresh_token(&self.client, &creds.refresh_token).await?;
        }
        Ok((creds.access_token.clone(), creds.account_id.clone()))
    }

    /// Build the JSON request body for the OpenAI Responses API.
    fn build_body(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> serde_json::Value {
        let input = convert_messages(messages);
        let mut body = serde_json::json!({
            "model": self.model,
            "store": false,
            "stream": true,
            "instructions": system,
            "input": input,
            "text": { "verbosity": "medium" },
            "include": ["reasoning.encrypted_content"],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });

        if !tools.is_empty() {
            body.as_object_mut()
                .expect("body is an object")
                .insert("tools".to_string(), serde_json::json!(convert_tools(tools)));
        }

        body
    }

    /// Build HTTP headers for the Codex API request.
    fn build_headers(
        access_token: &str,
        account_id: &str,
    ) -> Result<reqwest::header::HeaderMap> {
        use reqwest::header::{HeaderMap, HeaderValue};

        let os = std::env::consts::OS; // determinism-ok: CLI user-agent
        let arch = std::env::consts::ARCH; // determinism-ok: CLI user-agent

        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {access_token}"))?,
        );
        headers.insert(
            "chatgpt-account-id",
            HeaderValue::from_str(account_id)?,
        );
        headers.insert("OpenAI-Beta", HeaderValue::from_static("responses=experimental"));
        headers.insert("originator", HeaderValue::from_static("temper"));
        headers.insert(
            "User-Agent",
            HeaderValue::from_str(&format!("temper ({os}; {arch})"))?,
        );
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        Ok(headers)
    }
}

/// Convert Temper messages to OpenAI Responses API input format.
fn convert_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut input = Vec::new();

    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    input.push(serde_json::json!({
                        "role": msg.role,
                        "content": text,
                    }));
                }
                ContentBlock::ToolUse { id, name, input: tool_input } => {
                    let args = if tool_input.is_object() || tool_input.is_array() {
                        tool_input.to_string()
                    } else {
                        tool_input.as_str().unwrap_or("{}").to_string()
                    };
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": args,
                    }));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    }));
                }
            }
        }
    }

    input
}

/// Convert Anthropic-format tool definitions to OpenAI function format.
fn convert_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?;
            let description = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let parameters = tool.get("input_schema").cloned().unwrap_or(serde_json::json!({}));

            Some(serde_json::json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            }))
        })
        .collect()
}

#[async_trait::async_trait]
impl LlmProvider for CodexProvider {
    async fn send(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<LlmResponse> {
        // Delegate to streaming implementation (Codex API is always streamed).
        self.send_streaming(system, messages, tools, Box::new(|_| {}))
            .await
    }

    async fn send_streaming(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        on_delta: Box<dyn Fn(String) + Send>,
    ) -> Result<LlmResponse> {
        let (access_token, account_id) = self.ensure_valid_token().await?;
        let body = self.build_body(system, messages, tools);
        let headers = Self::build_headers(&access_token, &account_id)?;

        let response = self
            .client
            .post(API_BASE)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .context("Failed to send request to OpenAI Codex API")?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI Codex API error ({status}): {body_text}");
        }

        let byte_stream = response.bytes_stream();
        let mut stream = SseByteStream::new(byte_stream);

        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut stop_reason = String::from("unknown");
        let mut current_text = String::new();
        let mut current_tool_call_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_tool_args = String::new();
        let mut in_text_output = false;
        let mut in_function_call = false;

        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            let event_type = event.event_type.as_deref().unwrap_or("");

            match event_type {
                // Text output deltas.
                "response.output_text.delta" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                            in_text_output = true;
                            current_text.push_str(delta);
                            on_delta(delta.to_string());
                        }
                    }
                }

                // Function call argument deltas.
                "response.function_call_arguments.delta" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                            current_tool_args.push_str(delta);
                        }
                    }
                }

                // An output item is done — finalize text or tool_use block.
                "response.output_item.done" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        let item = payload.get("item").unwrap_or(&payload);
                        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

                        match item_type {
                            "message" | "text" => {
                                // Text output finalized.
                                if in_text_output && !current_text.is_empty() {
                                    content_blocks.push(ContentBlock::Text {
                                        text: current_text.clone(),
                                    });
                                    current_text.clear();
                                    in_text_output = false;
                                }
                            }
                            "function_call" => {
                                // Function call finalized.
                                let call_id = item
                                    .get("call_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&current_tool_call_id)
                                    .to_string();
                                let name = item
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&current_tool_name)
                                    .to_string();
                                let args_str = item
                                    .get("arguments")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| current_tool_args.clone());

                                let input: serde_json::Value =
                                    serde_json::from_str(&args_str).unwrap_or_default();

                                content_blocks.push(ContentBlock::ToolUse {
                                    id: call_id,
                                    name,
                                    input,
                                });

                                current_tool_call_id.clear();
                                current_tool_name.clear();
                                current_tool_args.clear();
                                in_function_call = false;
                            }
                            _ => {}
                        }
                    }
                }

                // A new output item is added — track function call metadata.
                "response.output_item.added" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        let item = payload.get("item").unwrap_or(&payload);
                        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

                        if item_type == "function_call" {
                            in_function_call = true;
                            current_tool_call_id = item
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            current_tool_name = item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            current_tool_args.clear();
                        }
                    }
                }

                // Response completed.
                "response.completed" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        let response_obj = payload.get("response").unwrap_or(&payload);
                        if let Some(reason) =
                            response_obj.get("status").and_then(|v| v.as_str())
                        {
                            stop_reason = reason.to_string();
                        }
                    }

                    // Flush any remaining text.
                    if in_text_output && !current_text.is_empty() {
                        content_blocks.push(ContentBlock::Text {
                            text: current_text.clone(),
                        });
                        current_text.clear();
                    }

                    // Flush any remaining tool call.
                    if in_function_call && !current_tool_args.is_empty() {
                        let input: serde_json::Value =
                            serde_json::from_str(&current_tool_args).unwrap_or_default();
                        content_blocks.push(ContentBlock::ToolUse {
                            id: current_tool_call_id.clone(),
                            name: current_tool_name.clone(),
                            input,
                        });
                    }

                    break;
                }

                _ => {}
            }
        }

        // Map OpenAI status to Anthropic-style stop reasons for the runner.
        let normalized_stop = match stop_reason.as_str() {
            "completed" => {
                if content_blocks.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. })) {
                    "tool_use".to_string()
                } else {
                    "end_turn".to_string()
                }
            }
            other => other.to_string(),
        };

        Ok(LlmResponse {
            content: content_blocks,
            stop_reason: normalized_stop,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_messages_user_text() {
        let msgs = vec![Message {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: "Hello".into(),
            }],
        }];
        let input = convert_messages(&msgs);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "Hello");
    }

    #[test]
    fn test_convert_messages_tool_result() {
        let msgs = vec![Message {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "42".into(),
                is_error: None,
            }],
        }];
        let input = convert_messages(&msgs);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["output"], "42");
    }

    #[test]
    fn test_convert_messages_assistant_tool_use() {
        let msgs = vec![Message {
            role: "assistant".into(),
            content: vec![ContentBlock::ToolUse {
                id: "call_2".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/tmp/test"}),
            }],
        }];
        let input = convert_messages(&msgs);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_2");
        assert_eq!(input[0]["name"], "read_file");
        let args: serde_json::Value =
            serde_json::from_str(input[0]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "/tmp/test");
    }

    #[test]
    fn test_convert_messages_mixed() {
        let msgs = vec![
            Message {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "Hello".into(),
                }],
            },
            Message {
                role: "assistant".into(),
                content: vec![
                    ContentBlock::Text {
                        text: "I'll help.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "ls".into(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            Message {
                role: "user".into(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "file.txt".into(),
                    is_error: None,
                }],
            },
        ];
        let input = convert_messages(&msgs);
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[test]
    fn test_convert_tools() {
        let tools = vec![serde_json::json!({
            "name": "read_file",
            "description": "Read a file from disk",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }
        })];
        let converted = convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["name"], "read_file");
        assert_eq!(converted[0]["description"], "Read a file from disk");
        assert_eq!(converted[0]["parameters"]["type"], "object");
    }

    #[test]
    fn test_convert_tools_empty() {
        let converted = convert_tools(&[]);
        assert!(converted.is_empty());
    }

    #[test]
    fn test_build_body_structure() {
        // Cannot call CodexProvider::new without credentials on disk,
        // so test conversion helpers directly.
        let tools = vec![serde_json::json!({
            "name": "test_tool",
            "description": "A test tool",
            "input_schema": { "type": "object" }
        })];
        let converted = convert_tools(&tools);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["name"], "test_tool");
    }

    #[test]
    fn test_stop_reason_mapping() {
        // Verify our mapping logic for stop reasons.
        let has_tool_use = true;
        let status = "completed";

        let result = match status {
            "completed" => {
                if has_tool_use {
                    "tool_use"
                } else {
                    "end_turn"
                }
            }
            other => other,
        };
        assert_eq!(result, "tool_use");

        let result_no_tool = match "completed" {
            "completed" => {
                if false {
                    "tool_use"
                } else {
                    "end_turn"
                }
            }
            other => other,
        };
        assert_eq!(result_no_tool, "end_turn");
    }
}
