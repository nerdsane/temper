//! Claude API client for the Anthropic Messages API.

use serde::{Deserialize, Serialize};
use serde_json::Value;

const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages";

/// A message in a Claude conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// Claude API response.
#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

/// Call the Anthropic Claude API.
pub async fn call_claude(
    api_key: &str,
    system_prompt: &str,
    messages: &[Message],
    model: &str,
) -> Result<String, String> {
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1024,
        "system": system_prompt,
        "messages": messages,
    });

    let resp = client
        .post(CLAUDE_API_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Claude API error: {e}"))?;

    let status = resp.status();
    let resp_body: Value = resp.json().await
        .map_err(|e| format!("Claude response parse error: {e}"))?;

    if !status.is_success() {
        let err = resp_body.get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown API error");
        return Err(format!("Claude API {}: {}", status, err));
    }

    // Extract text from response
    let text = resp_body.get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks.iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
        })
        .unwrap_or("")
        .to_string();

    Ok(text)
}
