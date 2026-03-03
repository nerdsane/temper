//! Anthropic Messages API client.
//!
//! Minimal client using `reqwest` for the Anthropic Messages API.
//! Reads `ANTHROPIC_API_KEY` from the environment.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
        let api_key =
            std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model: model.to_string(),
        })
    }

    /// Send a messages request and return the parsed response.
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
}
