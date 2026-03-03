//! Claude API client for conversational AI.
//!
//! Wraps the Anthropic Messages API to provide chat completions for the
//! developer interview and production agents.

use serde::{Deserialize, Serialize};

/// Error type for Claude API calls.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// API returned an error response.
    #[error("API error ({status}): {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Error message from the API.
        message: String,
    },
    /// Could not parse the API response.
    #[error("parse error: {0}")]
    Parse(String),
}

/// A chat message with role and content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Message role: "user" or "assistant".
    pub role: String,
    /// Message text content.
    pub content: String,
}

impl Message {
    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    /// Create an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

/// Trait for chat completion clients (real and mock).
///
/// Enables testing of Claude-powered agents without hitting the real API.
pub trait ChatClient: Send + Sync {
    /// Send a chat request and return the assistant's text response.
    fn chat(
        &self,
        messages: &[Message],
        system: &str,
    ) -> impl std::future::Future<Output = Result<String, ClaudeError>> + Send;
}

/// Client for the Anthropic Claude Messages API.
pub struct ClaudeClient {
    api_key: String,
    client: reqwest::Client,
    model: String,
}

/// Request body for the Claude Messages API.
#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: &'a [Message],
}

/// Response from the Claude Messages API.
#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

/// A content block in the API response.
#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    _block_type: String,
    text: Option<String>,
}

/// Error response from the API.
#[derive(Deserialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

/// Error detail in the API error response.
#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
}

impl ClaudeClient {
    /// Create a new Claude client with the default model.
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
            model: "claude-sonnet-4-5-20250929".to_string(),
        }
    }

    /// Create a new Claude client with a specific model.
    pub fn with_model(api_key: String, model: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
            model,
        }
    }

    /// Send a chat request to the Claude Messages API.
    ///
    /// Returns the assistant's text response.
    async fn chat_impl(&self, messages: &[Message], system: &str) -> Result<String, ClaudeError> {
        let request_body = MessagesRequest {
            model: &self.model,
            max_tokens: 4096,
            system,
            messages,
        };

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await?;

        let status = response.status().as_u16();
        if status != 200 {
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<ErrorResponse>(&body)
                .map(|e| e.error.message)
                .unwrap_or(body);
            return Err(ClaudeError::Api { status, message });
        }

        let resp: MessagesResponse = response.json().await?;
        let text = resp
            .content
            .into_iter()
            .filter_map(|block| block.text)
            .collect::<Vec<_>>()
            .join("");

        if text.is_empty() {
            return Err(ClaudeError::Parse(
                "no text content in response".to_string(),
            ));
        }

        Ok(text)
    }
}

impl ChatClient for ClaudeClient {
    async fn chat(&self, messages: &[Message], system: &str) -> Result<String, ClaudeError> {
        self.chat_impl(messages, system).await
    }
}

/// Mock Claude client for testing.
///
/// Returns canned responses in order. When all responses are consumed,
/// returns the last one repeatedly. If no responses are configured,
/// returns a default placeholder.
pub struct MockClaudeClient {
    responses: std::sync::Mutex<Vec<String>>,
    default_response: String,
}

impl MockClaudeClient {
    /// Create a mock client with the given canned responses.
    ///
    /// Responses are returned in FIFO order. When exhausted, returns
    /// the `default_response`.
    pub fn new(responses: Vec<String>, default_response: impl Into<String>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            default_response: default_response.into(),
        }
    }

    /// Create a mock that always returns the same response.
    pub fn fixed(response: impl Into<String>) -> Self {
        Self {
            responses: std::sync::Mutex::new(Vec::new()),
            default_response: response.into(),
        }
    }
}

impl ChatClient for MockClaudeClient {
    async fn chat(&self, _messages: &[Message], _system: &str) -> Result<String, ClaudeError> {
        let mut responses = self.responses.lock().unwrap_or_else(|e| e.into_inner());
        if responses.is_empty() {
            Ok(self.default_response.clone())
        } else {
            Ok(responses.remove(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_client_creation() {
        let client = ClaudeClient::new("test-key".to_string());
        assert_eq!(client.api_key, "test-key");
        assert_eq!(client.model, "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn test_claude_client_with_model() {
        let client =
            ClaudeClient::with_model("test-key".to_string(), "claude-opus-4-6".to_string());
        assert_eq!(client.model, "claude-opus-4-6");
    }

    #[test]
    fn test_message_constructors() {
        let user_msg = Message::user("hello");
        assert_eq!(user_msg.role, "user");
        assert_eq!(user_msg.content, "hello");

        let asst_msg = Message::assistant("hi there");
        assert_eq!(asst_msg.role, "assistant");
        assert_eq!(asst_msg.content, "hi there");
    }

    #[tokio::test]
    async fn test_mock_client_fixed_response() {
        let mock = MockClaudeClient::fixed("I am a mock response");
        let result = mock.chat(&[Message::user("hello")], "system").await;
        assert_eq!(result.unwrap(), "I am a mock response");

        // Second call returns the same fixed response.
        let result2 = mock.chat(&[Message::user("again")], "system").await;
        assert_eq!(result2.unwrap(), "I am a mock response");
    }

    #[tokio::test]
    async fn test_mock_client_sequential_responses() {
        let mock = MockClaudeClient::new(
            vec!["first".to_string(), "second".to_string()],
            "default".to_string(),
        );

        let r1 = mock.chat(&[Message::user("q1")], "sys").await.unwrap();
        assert_eq!(r1, "first");

        let r2 = mock.chat(&[Message::user("q2")], "sys").await.unwrap();
        assert_eq!(r2, "second");

        // Exhausted, returns default.
        let r3 = mock.chat(&[Message::user("q3")], "sys").await.unwrap();
        assert_eq!(r3, "default");
    }

    #[tokio::test]
    async fn test_mock_client_empty_returns_default() {
        let mock = MockClaudeClient::new(vec![], "fallback");
        let result = mock.chat(&[], "sys").await.unwrap();
        assert_eq!(result, "fallback");
    }
}
