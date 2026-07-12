//! Shared helpers for temper-agents actors.

use serde_json::Value;
use temper_actor_runtime::spec_actor::{SpecActorState, SpecMessage};
use temper_actor_runtime::{ActorContext, Message};

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

/// Best-effort append of a Message entity into the PG actor store.
///
/// Each message is stored as a `Message` actor under namespace:
/// `{process_namespace}/message/{message_id}`.
///
/// This is intentionally fire-and-forget: message persistence must not break the
/// agentic loop.
pub async fn append_message(
    ctx: &ActorContext,
    process_namespace: &str,
    role: &str,
    content: impl Into<String>,
) {
    let message_id = uuid::Uuid::new_v4().to_string();
    let message_namespace = format!("{process_namespace}/message/{message_id}");
    let process_id = session_id_from_namespace(process_namespace).to_string();
    let fields = serde_json::json!({
        "id": message_id,
        "process_id": process_id,
        "role": role,
        "content": content.into(),
        "created_at": chrono::Utc::now().to_rfc3339(),
    });

    let state = SpecActorState {
        status: "Written".to_string(),
        counters: Default::default(),
        booleans: Default::default(),
        lists: Default::default(),
        fields,
    };
    if let Ok(bytes) = serde_json::to_vec(&state) {
        // ARN-215: Message rows live under a child namespace; grant before write.
        ctx.grant_cross_namespace(message_namespace.clone());
        let _ = ctx
            .upsert_actor_state(&message_namespace, "Message", bytes)
            .await;
    }
}
