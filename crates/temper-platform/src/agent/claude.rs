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
    pub async fn chat(&self, messages: &[Message], system: &str) -> Result<String, ClaudeError> {
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
}
