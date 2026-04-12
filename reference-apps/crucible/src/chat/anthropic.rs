//! Minimal Anthropic Messages API client + deterministic mock.
//!
//! Phase 4 talks to Anthropic's **public** `/v1/messages` Messages API
//! (not the Managed Agents beta). The request/response shapes below
//! only include the fields the turn loop in
//! [`crate::chat::responder`] actually reads or writes — `system`,
//! `messages`, `model`, `max_tokens` on the request; `content`,
//! `stop_reason`, `usage` on the response.
//!
//! The [`Model`] trait lets the responder swap in a deterministic
//! [`MockModel`] when `CRUCIBLE_RESPONDER_MODE=mock`. This keeps the
//! integration test offline and lets a developer try the full loop
//! end-to-end without an API key.

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use super::openai::OpenAIModel;

/// The endpoint the real Anthropic client posts to. Kept as a const
/// rather than a config knob because Phase 4 is not in the business
/// of targeting alternate Anthropic environments.
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

// ----------------------------------------------------------------------
// Request / response wire shapes
// ----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String, // "user" | "assistant"
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }

    pub fn as_text(&self) -> &str {
        match self {
            ContentBlock::Text { text } => text,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessagesResponse {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_creation_input_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: i64,
}

impl MessagesResponse {
    /// Concatenate every text block in the response into a single
    /// string. Phase 4 only deals with text — no tool_use blocks, no
    /// thinking blocks — so this is exactly what
    /// `crate::chat::responder` needs to emit as an `agent.message`.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block.as_text());
        }
        out
    }
}

// ----------------------------------------------------------------------
// Model trait
// ----------------------------------------------------------------------

/// The single call the responder makes against the model provider.
///
/// This trait exists purely so `MockModel` and `AnthropicModel` can
/// be used interchangeably. It is generic — the responder is
/// polymorphic over `M: Model` rather than holding a `dyn Model`,
/// keeping both the trait and the call sites free of async-fn-in-dyn
/// ergonomics.
pub trait Model: Send + Sync {
    fn complete(
        &self,
        req: MessagesRequest,
    ) -> impl std::future::Future<Output = Result<MessagesResponse>> + Send;
}

// ----------------------------------------------------------------------
// Real Anthropic client
// ----------------------------------------------------------------------

pub struct AnthropicModel {
    api_key: String,
    http: Client,
}

impl AnthropicModel {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            http: Client::new(),
        }
    }
}

impl Model for AnthropicModel {
    fn complete(
        &self,
        req: MessagesRequest,
    ) -> impl std::future::Future<Output = Result<MessagesResponse>> + Send {
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        async move {
            let resp = http
                .post(ANTHROPIC_MESSAGES_URL)
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_API_VERSION)
                .header("content-type", "application/json")
                .json(&req)
                .send()
                .await
                .context("POST https://api.anthropic.com/v1/messages send failed")?;
            let status = resp.status();
            let text = resp
                .text()
                .await
                .context("reading Anthropic response body")?;
            if !status.is_success() {
                return Err(anyhow!("Anthropic {status}: {text}"));
            }
            serde_json::from_str::<MessagesResponse>(&text)
                .with_context(|| format!("decoding Anthropic response: {text}"))
        }
    }
}

// ----------------------------------------------------------------------
// Deterministic mock provider
// ----------------------------------------------------------------------

/// An in-process, deterministic model used when
/// `CRUCIBLE_RESPONDER_MODE=mock`. It captures the most recent
/// [`MessagesRequest`] it saw in a `Mutex` so integration tests can
/// assert on multi-turn history reconstruction.
///
/// The reply is `"Echo: <last user text>"` (or just `"Echo: (empty)"`
/// if the request has no user turns). Usage counts are synthesized
/// as `input_tokens = total_user_chars`, `output_tokens = reply.len()`
/// — small, non-zero, and deterministic. `stop_reason` is always
/// `"end_turn"`, matching the happy-path contract the responder
/// needs for `StatusIdleRequiresStopReason`.
#[derive(Default)]
pub struct MockModel {
    last_request: Mutex<Option<MessagesRequest>>,
}

impl MockModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_request(&self) -> Option<MessagesRequest> {
        self.last_request.lock().unwrap().clone()
    }
}

impl Model for MockModel {
    fn complete(
        &self,
        req: MessagesRequest,
    ) -> impl std::future::Future<Output = Result<MessagesResponse>> + Send {
        // Do all the work synchronously; the returned future is a
        // trivial ready-now — Send automatically because the closure
        // captures nothing beyond owned values.
        let mut last_user_text = String::from("(empty)");
        let mut total_user_chars: i64 = 0;
        for msg in &req.messages {
            if msg.role == "user" {
                for block in &msg.content {
                    let text = block.as_text();
                    total_user_chars += text.chars().count() as i64;
                    last_user_text.clear();
                    last_user_text.push_str(text);
                }
            }
        }
        let reply = format!("Echo: {last_user_text}");
        let output_tokens = reply.chars().count() as i64;

        *self.last_request.lock().unwrap() = Some(req);

        let resp = MessagesResponse {
            content: vec![ContentBlock::text(reply)],
            stop_reason: Some("end_turn".to_string()),
            usage: Usage {
                input_tokens: total_user_chars.max(1),
                output_tokens: output_tokens.max(1),
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        };
        async move { Ok(resp) }
    }
}

// ----------------------------------------------------------------------
// Provider selection
// ----------------------------------------------------------------------

/// Which provider [`model_from_env`] resolved to. The CLI and
/// integration test use this to decide which concrete `Model` they
/// got so they can call `.last_request()` in mock mode for
/// assertions.
pub enum ResolvedModel {
    Anthropic(AnthropicModel),
    OpenAI(OpenAIModel),
    Mock(MockModel),
}

/// Read `CRUCIBLE_RESPONDER_MODE`, `CRUCIBLE_RESPONDER_PROVIDER`, and
/// the matching API-key env var from the process environment and
/// return the right provider.
///
/// Rules (checked in order):
/// - `CRUCIBLE_RESPONDER_MODE=mock` → always [`MockModel`] regardless
///   of provider selection. This is how the integration test and the
///   offline demo both run without an API key.
/// - `CRUCIBLE_RESPONDER_PROVIDER=openai` → [`OpenAIModel`] reading
///   `OPENAI_API_KEY`.
/// - `CRUCIBLE_RESPONDER_PROVIDER=anthropic` (or unset) →
///   [`AnthropicModel`] reading `ANTHROPIC_API_KEY`.
/// - any other provider value → error.
pub fn model_from_env() -> Result<ResolvedModel> {
    let mode = std::env::var("CRUCIBLE_RESPONDER_MODE").unwrap_or_default();
    if mode == "mock" {
        return Ok(ResolvedModel::Mock(MockModel::new()));
    }
    let provider =
        std::env::var("CRUCIBLE_RESPONDER_PROVIDER").unwrap_or_else(|_| "anthropic".to_string());
    match provider.as_str() {
        "anthropic" => {
            let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                anyhow!(
                    "ANTHROPIC_API_KEY is not set. Either export it, set \
                     CRUCIBLE_RESPONDER_PROVIDER=openai together with \
                     OPENAI_API_KEY, or set CRUCIBLE_RESPONDER_MODE=mock \
                     to use the deterministic mock provider."
                )
            })?;
            if key.trim().is_empty() {
                return Err(anyhow!(
                    "ANTHROPIC_API_KEY is empty. Either export a real key or set \
                     CRUCIBLE_RESPONDER_MODE=mock to use the deterministic mock provider."
                ));
            }
            Ok(ResolvedModel::Anthropic(AnthropicModel::new(key)))
        }
        "openai" => {
            let key = std::env::var("OPENAI_API_KEY").map_err(|_| {
                anyhow!(
                    "CRUCIBLE_RESPONDER_PROVIDER=openai but OPENAI_API_KEY is not set. \
                     Export it, or set CRUCIBLE_RESPONDER_MODE=mock to use the \
                     deterministic mock provider."
                )
            })?;
            if key.trim().is_empty() {
                return Err(anyhow!(
                    "OPENAI_API_KEY is empty. Either export a real key or set \
                     CRUCIBLE_RESPONDER_MODE=mock to use the deterministic mock provider."
                ));
            }
            // `OPENAI_BASE_URL` lets Crucible drive any OpenAI-compatible
            // endpoint (Fireworks, Together, OpenRouter, local Ollama, …)
            // without code changes. Default is the real OpenAI host.
            let model = match std::env::var("OPENAI_BASE_URL") {
                Ok(base) if !base.trim().is_empty() => OpenAIModel::new_with_base_url(key, base),
                _ => OpenAIModel::new(key),
            };
            Ok(ResolvedModel::OpenAI(model))
        }
        other => Err(anyhow!(
            "Unknown CRUCIBLE_RESPONDER_PROVIDER={other:?}; expected 'anthropic' or 'openai'."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_echoes_last_user_message() {
        let m = MockModel::new();
        let req = MessagesRequest {
            model: "test".into(),
            system: Some("sys".into()),
            messages: vec![
                ChatMessage {
                    role: "user".into(),
                    content: vec![ContentBlock::text("hi")],
                },
                ChatMessage {
                    role: "assistant".into(),
                    content: vec![ContentBlock::text("hello")],
                },
                ChatMessage {
                    role: "user".into(),
                    content: vec![ContentBlock::text("what is 2+2?")],
                },
            ],
            max_tokens: 1024,
        };
        let resp = m.complete(req).await.unwrap();
        assert_eq!(resp.text(), "Echo: what is 2+2?");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert!(resp.usage.input_tokens > 0);
        assert!(resp.usage.output_tokens > 0);
    }

    #[tokio::test]
    async fn mock_captures_last_request_for_inspection() {
        let m = MockModel::new();
        let req = MessagesRequest {
            model: "test".into(),
            system: None,
            messages: vec![ChatMessage {
                role: "user".into(),
                content: vec![ContentBlock::text("probe")],
            }],
            max_tokens: 8,
        };
        let _ = m.complete(req.clone()).await.unwrap();
        let seen = m.last_request().expect("mock should capture request");
        assert_eq!(seen.messages.len(), 1);
        assert_eq!(seen.messages[0].role, "user");
        assert_eq!(seen.messages[0].content[0].as_text(), "probe");
    }

    #[test]
    fn content_block_round_trips_json() {
        let block = ContentBlock::text("hi");
        let j = serde_json::to_string(&block).unwrap();
        assert_eq!(j, r#"{"type":"text","text":"hi"}"#);
        let back: ContentBlock = serde_json::from_str(&j).unwrap();
        assert_eq!(back, block);
    }
}
