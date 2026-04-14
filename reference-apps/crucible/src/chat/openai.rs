//! Minimal OpenAI Chat Completions client for Phase 4.
//!
//! Translates between the neutral [`MessagesRequest`]/[`MessagesResponse`]
//! types in [`super::anthropic`] and OpenAI's
//! `/v1/chat/completions` wire shape. The translation surface is
//! deliberately small — only the fields the responder's 12-step loop
//! reads or writes end up on the wire.
//!
//! Mapping (Anthropic ↔ OpenAI):
//!
//! | Field        | Anthropic               | OpenAI                              |
//! | ------------ | ----------------------- | ----------------------------------- |
//! | endpoint     | `/v1/messages`          | `/v1/chat/completions`              |
//! | auth         | `x-api-key: <key>`      | `Authorization: Bearer <key>`       |
//! | system       | top-level `system`      | first message with `role:"system"`  |
//! | content      | `[{type:"text", text}]` | plain string                        |
//! | finish       | `stop_reason`           | `choices[0].finish_reason`          |
//! | usage        | `input/output_tokens`   | `prompt_tokens/completion_tokens`   |
//!
//! Provider selection is done by
//! [`super::anthropic::model_from_env`]: set
//! `CRUCIBLE_RESPONDER_PROVIDER=openai` and export `OPENAI_API_KEY`.
//!
//! ## OpenAI-compatible providers
//!
//! Because the wire shape is a community standard, every service that
//! advertises an "OpenAI-compatible API" can be driven by this same
//! client — Fireworks, Together, OpenRouter, Groq, local Ollama, etc.
//! Point the client at the provider's base URL via `OPENAI_BASE_URL`
//! (or the `--openai-base-url` CLI flag) and set the model id
//! accordingly. Default is the real OpenAI endpoint.
//! Examples:
//!
//! ```text
//! # Real OpenAI
//! OPENAI_BASE_URL unset (defaults to https://api.openai.com/v1)
//! --model gpt-4o-mini
//!
//! # Fireworks
//! OPENAI_BASE_URL=https://api.fireworks.ai/inference/v1
//! --model accounts/fireworks/models/kimi-k2p5-turbo
//! ```

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::anthropic::{ContentBlock, MessagesRequest, MessagesResponse, Model, Usage};

/// The default base URL used when `OPENAI_BASE_URL` is unset. The
/// full endpoint is `<base>/chat/completions`. Crucible keeps the
/// `/v1` prefix inside the base URL so third-party providers
/// (Fireworks, Together, OpenRouter, …) that ship their own versioned
/// prefix can be plugged in without string-surgery on the client.
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

// ----------------------------------------------------------------------
// OpenAI wire shapes (internal — never exposed outside this module)
// ----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAITool {
    #[serde(rename = "type")]
    ty: String,
    function: OpenAIToolFunction,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAIToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    ty: String,
    function: OpenAIFunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAIResponse {
    #[serde(default)]
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OpenAIChoice {
    #[serde(default)]
    message: OpenAIChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OpenAIChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OpenAIUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
}

// ----------------------------------------------------------------------
// Pure translation helpers (unit-tested)
// ----------------------------------------------------------------------

/// Flatten text content blocks into the single string OpenAI expects
/// for regular messages. Non-text blocks are skipped.
fn collapse_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if let Some(t) = b.as_text() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

/// Map a neutral [`MessagesRequest`] onto OpenAI's Chat Completions
/// request body. Handles system prompts, regular messages, tool_use
/// blocks (→ `tool_calls`), and tool_result blocks (→ `role:"tool"`).
fn request_to_openai(req: &MessagesRequest) -> OpenAIRequest {
    let mut messages: Vec<OpenAIMessage> = Vec::with_capacity(req.messages.len() + 1);
    if let Some(sys) = &req.system {
        messages.push(OpenAIMessage {
            role: "system".to_string(),
            content: Some(sys.clone()),
            tool_calls: None,
            tool_call_id: None,
        });
    }
    for m in &req.messages {
        let has_tool_results = m
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
        let has_tool_use = m.content.iter().any(|b| b.is_tool_use());

        if has_tool_results {
            // Each tool_result becomes a separate "tool" role message.
            for block in &m.content {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = block
                {
                    messages.push(OpenAIMessage {
                        role: "tool".to_string(),
                        content: Some(content.clone()),
                        tool_calls: None,
                        tool_call_id: Some(tool_use_id.clone()),
                    });
                }
            }
        } else if has_tool_use {
            // Assistant message with tool_calls.
            let text = collapse_blocks_to_text(&m.content);
            let tool_calls: Vec<OpenAIToolCall> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => Some(OpenAIToolCall {
                        id: id.clone(),
                        ty: "function".to_string(),
                        function: OpenAIFunctionCall {
                            name: name.clone(),
                            arguments: serde_json::to_string(input).unwrap_or_default(),
                        },
                    }),
                    _ => None,
                })
                .collect();
            messages.push(OpenAIMessage {
                role: m.role.clone(),
                content: if text.is_empty() { None } else { Some(text) },
                tool_calls: Some(tool_calls),
                tool_call_id: None,
            });
        } else {
            // Plain text message.
            messages.push(OpenAIMessage {
                role: m.role.clone(),
                content: Some(collapse_blocks_to_text(&m.content)),
                tool_calls: None,
                tool_call_id: None,
            });
        }
    }

    let tools = req.tools.as_ref().map(|defs| {
        defs.iter()
            .map(|d| OpenAITool {
                ty: "function".to_string(),
                function: OpenAIToolFunction {
                    name: d.name.clone(),
                    description: d.description.clone(),
                    parameters: d.input_schema.clone(),
                },
            })
            .collect()
    });

    OpenAIRequest {
        model: req.model.clone(),
        messages,
        max_tokens: req.max_tokens,
        tools,
    }
}

/// Map OpenAI's `finish_reason` onto one of the strings the Phase 3
/// `StatusIdleStopReasonWhenPresent` field invariant accepts
/// (`end_turn`, `max_tokens`, `stop_sequence`, `tool_use`,
/// `pause_turn`, `refusal`). Unknown values fall through to
/// `end_turn` so a surprise value from OpenAI doesn't trip the
/// `session.status_idle` write.
fn map_finish_reason(fr: Option<&str>) -> String {
    match fr {
        Some("stop") => "end_turn".to_string(),
        Some("length") => "max_tokens".to_string(),
        Some("content_filter") => "refusal".to_string(),
        Some("tool_calls") | Some("function_call") => "tool_use".to_string(),
        Some(_) | None => "end_turn".to_string(),
    }
}

/// Decode an OpenAI response into the neutral shape the responder
/// expects. Handles both plain text responses and tool_calls.
fn response_from_openai(body: OpenAIResponse) -> Result<MessagesResponse> {
    let choice = body
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("OpenAI response had no choices"))?;
    let stop_reason = map_finish_reason(choice.finish_reason.as_deref());
    let usage = body.usage.unwrap_or_default();

    let mut content = Vec::new();
    if let Some(text) = choice.message.content
        && !text.is_empty()
    {
        content.push(ContentBlock::text(text));
    }
    if let Some(tool_calls) = choice.message.tool_calls {
        for tc in tool_calls {
            let input: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}));
            content.push(ContentBlock::ToolUse {
                id: tc.id,
                name: tc.function.name,
                input,
            });
        }
    }
    if content.is_empty() {
        content.push(ContentBlock::text(""));
    }

    Ok(MessagesResponse {
        content,
        stop_reason: Some(stop_reason),
        usage: Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    })
}

// ----------------------------------------------------------------------
// OpenAIModel — implements the neutral Model trait
// ----------------------------------------------------------------------

pub struct OpenAIModel {
    api_key: String,
    base_url: String,
    http: Client,
}

impl OpenAIModel {
    /// Construct a client pointed at the real OpenAI endpoint.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::new_with_base_url(api_key, DEFAULT_OPENAI_BASE_URL)
    }

    /// Construct a client pointed at an arbitrary OpenAI-compatible
    /// base URL. Trailing slashes on `base_url` are trimmed so either
    /// `https://api.fireworks.ai/inference/v1` or
    /// `https://api.fireworks.ai/inference/v1/` works.
    pub fn new_with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let base = base_url.into();
        let base = base.trim_end_matches('/').to_string();
        Self {
            api_key: api_key.into(),
            base_url: base,
            http: Client::new(),
        }
    }

    /// The full chat-completions URL this client will POST to.
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

impl Model for OpenAIModel {
    fn complete(
        &self,
        req: MessagesRequest,
    ) -> impl std::future::Future<Output = Result<MessagesResponse>> + Send {
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        let url = self.chat_url();
        let outgoing = request_to_openai(&req);
        async move {
            let resp = http
                .post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("content-type", "application/json")
                .json(&outgoing)
                .send()
                .await
                .with_context(|| format!("POST {url} send failed"))?;
            let status = resp.status();
            let text = resp.text().await.context("reading OpenAI response body")?;
            if !status.is_success() {
                return Err(anyhow!("OpenAI-compatible provider {status}: {text}"));
            }
            let parsed: OpenAIResponse = serde_json::from_str(&text)
                .with_context(|| format!("decoding OpenAI-compatible response: {text}"))?;
            response_from_openai(parsed)
        }
    }
}

// ----------------------------------------------------------------------
// Unit tests — pure translation, no network
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::anthropic::ChatMessage;

    fn user(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: vec![ContentBlock::text(text)],
        }
    }

    fn assistant(text: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: vec![ContentBlock::text(text)],
        }
    }

    #[test]
    fn request_includes_system_prompt_as_first_message() {
        let req = MessagesRequest {
            model: "gpt-4o-mini".into(),
            system: Some("you are a concise assistant".into()),
            messages: vec![user("hello"), assistant("hi"), user("what is 2+2?")],
            max_tokens: 512,
            tools: None,
        };
        let out = request_to_openai(&req);
        assert_eq!(out.model, "gpt-4o-mini");
        assert_eq!(out.max_tokens, 512);
        assert_eq!(out.messages.len(), 4);
        assert_eq!(out.messages[0].role, "system");
        assert_eq!(
            out.messages[0].content.as_deref(),
            Some("you are a concise assistant")
        );
        assert_eq!(out.messages[1].role, "user");
        assert_eq!(out.messages[1].content.as_deref(), Some("hello"));
        assert_eq!(out.messages[2].role, "assistant");
        assert_eq!(out.messages[2].content.as_deref(), Some("hi"));
        assert_eq!(out.messages[3].role, "user");
        assert_eq!(out.messages[3].content.as_deref(), Some("what is 2+2?"));
    }

    #[test]
    fn request_without_system_has_no_system_message() {
        let req = MessagesRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![user("probe")],
            max_tokens: 16,
            tools: None,
        };
        let out = request_to_openai(&req);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].role, "user");
        assert_eq!(out.messages[0].content.as_deref(), Some("probe"));
    }

    #[test]
    fn collapse_blocks_joins_multiple_text_blocks_with_newlines() {
        let blocks = vec![ContentBlock::text("first"), ContentBlock::text("second")];
        assert_eq!(collapse_blocks_to_text(&blocks), "first\nsecond");
    }

    #[test]
    fn collapse_blocks_empty_produces_empty_string() {
        assert_eq!(collapse_blocks_to_text(&[]), "");
    }

    #[test]
    fn response_from_openai_extracts_text_and_stop_reason() {
        let body = OpenAIResponse {
            choices: vec![OpenAIChoice {
                message: OpenAIChoiceMessage {
                    content: Some("2 + 2 equals 4.".to_string()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OpenAIUsage {
                prompt_tokens: 42,
                completion_tokens: 9,
            }),
        };
        let resp = response_from_openai(body).unwrap();
        assert_eq!(resp.text(), "2 + 2 equals 4.");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.usage.input_tokens, 42);
        assert_eq!(resp.usage.output_tokens, 9);
    }

    #[test]
    fn response_from_openai_maps_length_to_max_tokens() {
        let body = OpenAIResponse {
            choices: vec![OpenAIChoice {
                message: OpenAIChoiceMessage {
                    content: Some("truncated".to_string()),
                    tool_calls: None,
                },
                finish_reason: Some("length".to_string()),
            }],
            usage: None,
        };
        let resp = response_from_openai(body).unwrap();
        assert_eq!(resp.stop_reason.as_deref(), Some("max_tokens"));
        assert_eq!(resp.usage.input_tokens, 0);
        assert_eq!(resp.usage.output_tokens, 0);
    }

    #[test]
    fn response_from_openai_handles_tool_calls() {
        let body = OpenAIResponse {
            choices: vec![OpenAIChoice {
                message: OpenAIChoiceMessage {
                    content: Some("Let me check.".to_string()),
                    tool_calls: Some(vec![OpenAIToolCall {
                        id: "call_1".into(),
                        ty: "function".into(),
                        function: OpenAIFunctionCall {
                            name: "bash".into(),
                            arguments: r#"{"command":"ls"}"#.into(),
                        },
                    }]),
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        };
        let resp = response_from_openai(body).unwrap();
        assert_eq!(resp.text(), "Let me check.");
        assert!(resp.has_tool_use());
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn response_from_openai_errors_on_empty_choices() {
        let body = OpenAIResponse {
            choices: vec![],
            usage: None,
        };
        assert!(response_from_openai(body).is_err());
    }

    #[test]
    fn chat_url_defaults_to_openai() {
        let m = OpenAIModel::new("ignored");
        assert_eq!(m.chat_url(), "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn chat_url_trims_trailing_slash_on_base() {
        let m = OpenAIModel::new_with_base_url("ignored", "https://api.fireworks.ai/inference/v1/");
        assert_eq!(
            m.chat_url(),
            "https://api.fireworks.ai/inference/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_honours_custom_base() {
        let m = OpenAIModel::new_with_base_url("ignored", "https://api.fireworks.ai/inference/v1");
        assert_eq!(
            m.chat_url(),
            "https://api.fireworks.ai/inference/v1/chat/completions"
        );
    }

    #[test]
    fn map_finish_reason_covers_every_known_value() {
        assert_eq!(map_finish_reason(Some("stop")), "end_turn");
        assert_eq!(map_finish_reason(Some("length")), "max_tokens");
        assert_eq!(map_finish_reason(Some("content_filter")), "refusal");
        assert_eq!(map_finish_reason(Some("tool_calls")), "tool_use");
        assert_eq!(map_finish_reason(Some("function_call")), "tool_use");
        // Unknown and missing both collapse to end_turn so the
        // Phase 3 StatusIdleStopReasonWhenPresent invariant still
        // accepts the downstream session.status_idle row.
        assert_eq!(map_finish_reason(Some("surprise_value")), "end_turn");
        assert_eq!(map_finish_reason(None), "end_turn");
    }
}
