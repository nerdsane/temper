//! Shared helpers for temper-agents actors.

use serde_json::Value;
use temper_actor_runtime::Message;
use temper_actor_runtime::spec_actor::SpecMessage;

/// Decode params from a SpecMessage.
pub fn decode_params(message: &Message) -> Value {
    message
        .decode::<SpecMessage>()
        .ok()
        .and_then(|m| {
            if m.params.is_empty() {
                None
            } else {
                serde_json::from_slice::<Value>(&m.params).ok()
            }
        })
        .unwrap_or(serde_json::json!({}))
}

/// Extract the action name from a message (handles SpecMessage wrapping).
pub fn message_action(message: &Message) -> String {
    if message.message_type.ends_with("SpecMessage") {
        message
            .decode::<SpecMessage>()
            .ok()
            .filter(|m| !m.action.is_empty())
            .map(|m| m.action)
            .unwrap_or_else(|| message.message_type.clone())
    } else {
        message.message_type.clone()
    }
}

/// Extract session_id from namespace format "{tenant}/{session_id}".
pub fn session_id_from_namespace(namespace: &str) -> &str {
    namespace.split('/').next_back().unwrap_or(namespace)
}
