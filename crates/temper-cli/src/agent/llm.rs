//! Anthropic Messages API client.
//!
//! Minimal client using `reqwest` for the Anthropic Messages API.
//! Reads `ANTHROPIC_API_KEY` from the environment.

use std::io::Write;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;

use super::tools::ToolDef;

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 4096;

/// A content block in the LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// Parsed LLM response.
#[derive(Debug)]
pub struct LlmResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
}

/// Message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

/// Anthropic Messages API client.
pub struct AnthropicClient {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicClient {
    /// Create a new client reading `ANTHROPIC_API_KEY` from the environment.
    pub fn new(model: &str) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model: model.to_string(),
        })
    }

    /// Send a messages request and return the parsed response (non-streaming).
    pub async fn send(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<LlmResponse> {
        let tool_schemas: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": messages,
            "tools": tool_schemas,
        });

        let response = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Anthropic API")?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error ({status}): {body_text}");
        }

        let resp_json: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse Anthropic API response")?;

        let stop_reason = resp_json
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let content: Vec<ContentBlock> = resp_json
            .get("content")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok(LlmResponse {
            content,
            stop_reason,
        })
    }

    /// Send a streaming messages request, printing text in real-time.
    ///
    /// Sets `"stream": true` in the Anthropic API request and parses SSE events.
    /// Text deltas are printed to stdout as they arrive. Tool-use JSON deltas are
    /// accumulated. Returns the same `LlmResponse` type as `send()`.
    pub async fn send_streaming(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<LlmResponse> {
        let tool_schemas: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": messages,
            "tools": tool_schemas,
            "stream": true,
        });

        let response = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send streaming request to Anthropic API")?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error ({status}): {body_text}");
        }

        // Parse the SSE stream from the response body.
        let byte_stream = response.bytes_stream();
        let mut stream = super::sse::SseStream::from_byte_stream(byte_stream);

        // Accumulate content blocks.
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut stop_reason = String::from("unknown");

        // Track current content block being built.
        let mut current_text = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_tool_json = String::new();
        let mut in_text_block = false;
        let mut in_tool_block = false;

        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            let event_type = event.event_type.as_deref().unwrap_or("");

            match event_type {
                "content_block_start" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        if let Some(block) = payload.get("content_block") {
                            let block_type =
                                block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match block_type {
                                "text" => {
                                    in_text_block = true;
                                    in_tool_block = false;
                                    current_text.clear();
                                }
                                "tool_use" => {
                                    in_text_block = false;
                                    in_tool_block = true;
                                    current_tool_id = block
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    current_tool_name = block
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    current_tool_json.clear();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "content_block_delta" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        if let Some(delta) = payload.get("delta") {
                            let delta_type =
                                delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match delta_type {
                                "text_delta" => {
                                    if in_text_block {
                                        if let Some(text) =
                                            delta.get("text").and_then(|v| v.as_str())
                                        {
                                            // Print text to stdout in real-time.
                                            print!("{text}");
                                            std::io::stdout().flush().ok();
                                            current_text.push_str(text);
                                        }
                                    }
                                }
                                "input_json_delta" => {
                                    if in_tool_block {
                                        if let Some(json_part) =
                                            delta.get("partial_json").and_then(|v| v.as_str())
                                        {
                                            current_tool_json.push_str(json_part);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "content_block_stop" => {
                    if in_text_block {
                        if !current_text.is_empty() {
                            content_blocks.push(ContentBlock::Text {
                                text: current_text.clone(),
                            });
                        }
                        in_text_block = false;
                        current_text.clear();
                    } else if in_tool_block {
                        let input: serde_json::Value =
                            serde_json::from_str(&current_tool_json).unwrap_or_default();
                        content_blocks.push(ContentBlock::ToolUse {
                            id: current_tool_id.clone(),
                            name: current_tool_name.clone(),
                            input,
                        });
                        in_tool_block = false;
                        current_tool_json.clear();
                    }
                }
                "message_delta" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        if let Some(delta) = payload.get("delta") {
                            if let Some(reason) = delta.get("stop_reason").and_then(|v| v.as_str())
                            {
                                stop_reason = reason.to_string();
                            }
                        }
                    }
                }
                "message_stop" => {
                    // End of message — break out of the loop.
                    break;
                }
                _ => {
                    // Ignore unknown event types (ping, error, etc.).
                }
            }
        }

        // Ensure trailing newline after streaming text.
        if content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { .. }))
        {
            println!();
        }

        Ok(LlmResponse {
            content: content_blocks,
            stop_reason,
        })
    }
}
