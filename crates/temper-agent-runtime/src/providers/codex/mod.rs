//! OpenAI Codex provider via ChatGPT Plus/Pro subscription OAuth.
//!
//! Implements [`LlmProvider`] for the OpenAI Responses API, authenticating
//! with tokens obtained via `temper login openai`.

pub mod auth;
mod convert;

use anyhow::{Context as _, Result};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

use super::sse::SseByteStream;
use super::{ContentBlock, LlmProvider, LlmResponse, Message};
use auth::{API_BASE, CodexCredentials};
use convert::{convert_messages, convert_tools};

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
    fn build_headers(access_token: &str, account_id: &str) -> Result<reqwest::header::HeaderMap> {
        use reqwest::header::{HeaderMap, HeaderValue};

        let os = std::env::consts::OS; // determinism-ok: CLI user-agent
        let arch = std::env::consts::ARCH; // determinism-ok: CLI user-agent

        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {access_token}"))?,
        );
        headers.insert("chatgpt-account-id", HeaderValue::from_str(account_id)?);
        headers.insert(
            "OpenAI-Beta",
            HeaderValue::from_static("responses=experimental"),
        );
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
                                if in_text_output && !current_text.is_empty() {
                                    content_blocks.push(ContentBlock::Text {
                                        text: current_text.clone(),
                                    });
                                    current_text.clear();
                                    in_text_output = false;
                                }
                            }
                            "function_call" => {
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
                        if let Some(reason) = response_obj.get("status").and_then(|v| v.as_str()) {
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
                if content_blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
                {
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
