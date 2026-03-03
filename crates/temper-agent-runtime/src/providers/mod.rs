//! LLM provider trait and implementations.
//!
//! The [`LlmProvider`] trait abstracts over different LLM backends.
//! Currently provides [`AnthropicProvider`] for the Anthropic Messages API.

pub mod anthropic;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A content block in an LLM message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// A text content block.
    #[serde(rename = "text")]
    Text { text: String },
    /// A tool use request from the LLM.
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool result sent back to the LLM.
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
    /// The content blocks in the response.
    pub content: Vec<ContentBlock>,
    /// The reason the LLM stopped generating.
    pub stop_reason: String,
}

/// A message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// The role: "user" or "assistant".
    pub role: String,
    /// The content blocks.
    pub content: Vec<ContentBlock>,
}

/// Trait for pluggable LLM providers.
///
/// Implementations handle the details of API calls, authentication, and
/// response parsing for a specific LLM backend.
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send a non-streaming request to the LLM.
    async fn send(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<LlmResponse>;

    /// Send a streaming request to the LLM.
    ///
    /// The `on_delta` callback is invoked with text deltas as they arrive,
    /// enabling real-time output. Takes `String` to avoid lifetime issues
    /// with `Box<dyn Fn>` drop ordering in Rust 2024 edition.
    async fn send_streaming(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        on_delta: Box<dyn Fn(String) + Send>,
    ) -> Result<LlmResponse>;
}
