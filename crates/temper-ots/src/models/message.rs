//! Message models for turns
//!
//! DST adaptation: `OTSMessage::new()` uses `sim_uuid()` for ID generation
//! and accepts a `DateTime<Utc>` timestamp parameter instead of calling
//! `Utc::now()`.

use crate::models::{ContentType, MessageRole};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::sim_uuid;

/// Content of a message
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSMessageContent {
    /// Content type
    #[serde(rename = "type")]
    pub content_type: ContentType,

    /// Structured data for tool calls/responses
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,

    /// Text content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Default for OTSMessageContent {
    fn default() -> Self {
        Self {
            content_type: ContentType::Text,
            data: None,
            text: None,
        }
    }
}

impl OTSMessageContent {
    /// Create text content
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content_type: ContentType::Text,
            data: None,
            text: Some(text.into()),
        }
    }

    /// Create tool call content
    pub fn tool_call(data: serde_json::Value) -> Self {
        Self {
            content_type: ContentType::ToolCall,
            data: Some(data),
            text: None,
        }
    }

    /// Create tool response content
    pub fn tool_response(data: serde_json::Value) -> Self {
        Self {
            content_type: ContentType::ToolResponse,
            data: Some(data),
            text: None,
        }
    }

    /// Create widget content
    pub fn widget(data: serde_json::Value) -> Self {
        Self {
            content_type: ContentType::Widget,
            data: Some(data),
            text: None,
        }
    }
}

/// Visibility controls for a message
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSVisibility {
    /// Whether message should be sent to user
    pub send_to_user: bool,

    /// Whether message should be persisted
    pub persist: bool,
}

impl Default for OTSVisibility {
    fn default() -> Self {
        Self {
            send_to_user: true,
            persist: true,
        }
    }
}

impl OTSVisibility {
    /// Create new visibility settings
    pub fn new(send_to_user: bool, persist: bool) -> Self {
        Self {
            send_to_user,
            persist,
        }
    }

    /// Create visibility for internal messages (not sent to user)
    pub fn internal() -> Self {
        Self {
            send_to_user: false,
            persist: true,
        }
    }

    /// Create visibility for ephemeral messages (not persisted)
    pub fn ephemeral() -> Self {
        Self {
            send_to_user: true,
            persist: false,
        }
    }
}

/// Context snapshot at a specific message
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSContextSnapshot {
    /// Entity IDs active at this point
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<String>,

    /// Tools available at this point
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_tools: Vec<String>,
}

impl Default for OTSContextSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

impl OTSContextSnapshot {
    /// Create a new empty context snapshot
    pub fn new() -> Self {
        Self {
            entities: Vec::new(),
            available_tools: Vec::new(),
        }
    }

    /// Add an entity ID
    pub fn with_entity(mut self, entity_id: impl Into<String>) -> Self {
        self.entities.push(entity_id.into());
        self
    }

    /// Add a tool name
    pub fn with_tool(mut self, tool_name: impl Into<String>) -> Self {
        self.available_tools.push(tool_name.into());
        self
    }
}

/// A single message in a turn
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSMessage {
    /// Unique message identifier
    pub message_id: String,

    /// Message role
    pub role: MessageRole,

    /// When message was created
    pub timestamp: DateTime<Utc>,

    /// Message content
    pub content: OTSMessageContent,

    /// Chain-of-thought reasoning (assistant only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,

    /// Visibility controls
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<OTSVisibility>,

    /// Context snapshot at this message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_snapshot: Option<OTSContextSnapshot>,
}

impl OTSMessage {
    /// Create a new message with the given role, content, and timestamp.
    ///
    /// Uses `sim_uuid()` for deterministic ID generation in simulation.
    /// Accepts an explicit timestamp instead of calling `Utc::now()`.
    pub fn new(role: MessageRole, content: OTSMessageContent, timestamp: DateTime<Utc>) -> Self {
        Self {
            message_id: sim_uuid().to_string(),
            role,
            timestamp,
            content,
            reasoning: None,
            visibility: None,
            context_snapshot: None,
        }
    }

    /// Set the message ID
    pub fn with_message_id(mut self, message_id: impl Into<String>) -> Self {
        self.message_id = message_id.into();
        self
    }

    /// Set the timestamp
    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = timestamp;
        self
    }

    /// Set the reasoning
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }

    /// Set the visibility
    pub fn with_visibility(mut self, visibility: OTSVisibility) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Set the context snapshot
    pub fn with_context_snapshot(mut self, context_snapshot: OTSContextSnapshot) -> Self {
        self.context_snapshot = Some(context_snapshot);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use temper_runtime::scheduler::sim_now;

    #[test]
    fn test_message_content_text() {
        let content = OTSMessageContent::text("Hello, world!");
        assert_eq!(content.content_type, ContentType::Text);
        assert_eq!(content.text, Some("Hello, world!".to_string()));
        assert_eq!(content.data, None);

        let json_str = serde_json::to_string(&content).unwrap();
        let parsed: OTSMessageContent = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, content);
    }

    #[test]
    fn test_message_content_tool_call() {
        let data = json!({"tool": "calculator", "args": {"x": 5}});
        let content = OTSMessageContent::tool_call(data.clone());
        assert_eq!(content.content_type, ContentType::ToolCall);
        assert_eq!(content.data, Some(data));

        let json_str = serde_json::to_string(&content).unwrap();
        let parsed: OTSMessageContent = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, content);
    }

    #[test]
    fn test_visibility_default() {
        let vis = OTSVisibility::default();
        assert!(vis.send_to_user);
        assert!(vis.persist);
    }

    #[test]
    fn test_visibility_internal() {
        let vis = OTSVisibility::internal();
        assert!(!vis.send_to_user);
        assert!(vis.persist);
    }

    #[test]
    fn test_visibility_ephemeral() {
        let vis = OTSVisibility::ephemeral();
        assert!(vis.send_to_user);
        assert!(!vis.persist);
    }

    #[test]
    fn test_context_snapshot() {
        let snapshot = OTSContextSnapshot::new()
            .with_entity("entity_1")
            .with_entity("entity_2")
            .with_tool("calculator")
            .with_tool("search");

        assert_eq!(snapshot.entities.len(), 2);
        assert_eq!(snapshot.available_tools.len(), 2);

        let json_str = serde_json::to_string(&snapshot).unwrap();
        let parsed: OTSContextSnapshot = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, snapshot);
    }

    #[test]
    fn test_message_serialization() {
        let now = sim_now();
        let content = OTSMessageContent::text("Test message");
        let visibility = OTSVisibility::internal();
        let snapshot = OTSContextSnapshot::new().with_tool("search");

        let message = OTSMessage::new(MessageRole::Assistant, content, now)
            .with_reasoning("This is my reasoning")
            .with_visibility(visibility)
            .with_context_snapshot(snapshot);

        let json_str = serde_json::to_string(&message).unwrap();
        let parsed: OTSMessage = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.role, message.role);
        assert_eq!(parsed.content, message.content);
        assert_eq!(parsed.reasoning, message.reasoning);
        assert_eq!(parsed.visibility, message.visibility);
        assert_eq!(parsed.context_snapshot, message.context_snapshot);
    }

    #[test]
    fn test_message_optional_fields_omitted() {
        let now = sim_now();
        let content = OTSMessageContent::text("Simple message");
        let message = OTSMessage::new(MessageRole::User, content, now);

        let json_str = serde_json::to_string(&message).unwrap();

        // Optional fields should not appear in JSON
        assert!(!json_str.contains("\"reasoning\""));
        assert!(!json_str.contains("\"visibility\""));
        assert!(!json_str.contains("\"context_snapshot\""));
    }

    #[test]
    fn test_empty_context_snapshot_omits_fields() {
        let snapshot = OTSContextSnapshot::new();
        let json_str = serde_json::to_string(&snapshot).unwrap();

        // Empty vecs should not appear
        assert_eq!(json_str, "{}");
    }
}
