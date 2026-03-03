//! Anthropic Messages API provider.
//!
//! Implements [`LlmProvider`] for the Anthropic Messages API with both
//! non-streaming and streaming support.

use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use futures_core::Stream;
use tokio_stream::StreamExt;

use super::{ContentBlock, LlmProvider, LlmResponse, Message};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 4096;

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    /// Create a new provider reading `ANTHROPIC_API_KEY` from the environment.
    pub fn new(model: &str) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?; // determinism-ok: CLI/executor code, not simulation-visible
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model: model.to_string(),
        })
    }

    /// Create a new provider with an explicit API key.
    pub fn with_key(api_key: &str, model: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            model: model.to_string(),
        }
    }

    /// Build the JSON request body.
    fn build_body(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        stream: bool,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": messages,
            "tools": tools,
        });
        if stream {
            body.as_object_mut()
                .expect("body is an object")
                .insert("stream".to_string(), serde_json::Value::Bool(true));
        }
        body
    }
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicProvider {
    async fn send(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<LlmResponse> {
        let body = self.build_body(system, messages, tools, false);

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

    async fn send_streaming(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        on_delta: Box<dyn Fn(String) + Send>,
    ) -> Result<LlmResponse> {
        let body = self.build_body(system, messages, tools, true);

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

        let byte_stream = response.bytes_stream();
        let mut stream = SseByteStream::new(byte_stream);

        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut stop_reason = String::from("unknown");
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
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data)
                        && let Some(block) = payload.get("content_block")
                    {
                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
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
                "content_block_delta" => {
                    let (text_delta, json_delta) = extract_deltas(&event.data);

                    if in_text_block && let Some(text) = text_delta {
                        current_text.push_str(&text);
                        on_delta(text);
                    }
                    if in_tool_block && let Some(json_part) = json_delta {
                        current_tool_json.push_str(&json_part);
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
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data)
                        && let Some(delta) = payload.get("delta")
                        && let Some(reason) = delta.get("stop_reason").and_then(|v| v.as_str())
                    {
                        stop_reason = reason.to_string();
                    }
                }
                "message_stop" => break,
                _ => {}
            }
        }

        Ok(LlmResponse {
            content: content_blocks,
            stop_reason,
        })
    }
}

// ── Internal helpers ────────────────────────────────────────────────────

/// Extract text and JSON deltas from an SSE data payload.
///
/// Returns `(text_delta, json_delta)` as owned Strings. Separating this from
/// the main loop avoids drop-order lifetime issues with `on_delta` closures
/// in Rust 2024 edition.
fn extract_deltas(data: &str) -> (Option<String>, Option<String>) {
    let payload = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(p) => p,
        Err(_) => return (None, None),
    };

    let delta = match payload.get("delta") {
        Some(d) => d,
        None => return (None, None),
    };

    let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match delta_type {
        "text_delta" => {
            let text = delta.get("text").and_then(|v| v.as_str()).map(String::from);
            (text, None)
        }
        "input_json_delta" => {
            let json_part = delta
                .get("partial_json")
                .and_then(|v| v.as_str())
                .map(String::from);
            (None, json_part)
        }
        _ => (None, None),
    }
}

// ── Internal SSE parser for streaming responses ──────────────────────────

/// A parsed SSE event from the Anthropic streaming API.
#[derive(Debug)]
struct SseEvent {
    event_type: Option<String>,
    data: String,
}

/// Minimal SSE parser over a byte stream.
struct SseByteStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buffer: String,
    current_event_type: Option<String>,
    current_data: Vec<String>,
}

impl SseByteStream {
    fn new(stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(stream),
            buffer: String::new(),
            current_event_type: None,
            current_data: Vec::new(),
        }
    }

    fn try_parse_event(&mut self) -> Option<SseEvent> {
        loop {
            let newline_pos = self.buffer.find('\n')?;
            let line = self.buffer[..newline_pos]
                .trim_end_matches('\r')
                .to_string();
            self.buffer = self.buffer[newline_pos + 1..].to_string();

            if line.is_empty() {
                if self.current_data.is_empty() {
                    self.current_event_type = None;
                    continue;
                }
                let event = SseEvent {
                    event_type: self.current_event_type.take(),
                    data: self.current_data.join("\n"),
                };
                self.current_data.clear();
                return Some(event);
            }

            if let Some(value) = line.strip_prefix("data:") {
                self.current_data.push(value.trim_start().to_string());
            } else if let Some(value) = line.strip_prefix("event:") {
                self.current_event_type = Some(value.trim_start().to_string());
            }
        }
    }
}

impl Stream for SseByteStream {
    type Item = Result<SseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.try_parse_event() {
            return Poll::Ready(Some(Ok(event)));
        }

        loop {
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    self.buffer.push_str(&text);

                    if let Some(event) = self.try_parse_event() {
                        return Poll::Ready(Some(Ok(event)));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e.into())));
                }
                Poll::Ready(None) => {
                    if !self.current_data.is_empty() {
                        let event = SseEvent {
                            event_type: self.current_event_type.take(),
                            data: self.current_data.join("\n"),
                        };
                        self.current_data.clear();
                        return Poll::Ready(Some(Ok(event)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_body_non_streaming() {
        let provider = AnthropicProvider::with_key("test-key", "test-model");
        let body = provider.build_body("system", &[], &[], false);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["system"], "system");
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn test_build_body_streaming() {
        let provider = AnthropicProvider::with_key("test-key", "test-model");
        let body = provider.build_body("system", &[], &[], true);
        assert_eq!(body["stream"], true);
    }
}
