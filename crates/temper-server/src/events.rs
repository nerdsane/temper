//! Server-Sent Events (SSE) for entity state change subscriptions.
//!
//! Provides a `/tdata/$events` endpoint that streams real-time entity
//! state transitions to connected clients via SSE.

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::state::ServerState;

/// A notification emitted when an entity transitions to a new state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityStateChange {
    /// The entity type (e.g., "Order").
    pub entity_type: String,
    /// The entity ID.
    pub entity_id: String,
    /// The action that triggered the transition.
    pub action: String,
    /// The new status after the transition.
    pub status: String,
    /// The tenant that owns the entity.
    pub tenant: String,
    /// Agent that performed the action (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Session in which the action was performed (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// SSE endpoint handler: streams entity state changes to connected clients.
///
/// Clients connect to `/tdata/$events` and receive a stream of JSON events
/// for every successful entity state transition.
pub async fn handle_events(
    State(state): State<ServerState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| {
        match result {
            Ok(change) => {
                let data = serde_json::to_string(&change).unwrap_or_default();
                Some(Ok(Event::default().event("state_change").data(data)))
            }
            // Lagged receiver: skip missed events and continue
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_state_change_serializes() {
        let change = EntityStateChange {
            entity_type: "Order".into(),
            entity_id: "o-1".into(),
            action: "SubmitOrder".into(),
            status: "Submitted".into(),
            tenant: "default".into(),
            agent_id: Some("agent-1".into()),
            session_id: None,
        };
        let json = serde_json::to_string(&change).unwrap();
        assert!(json.contains("\"entity_type\":\"Order\""));
        assert!(json.contains("\"action\":\"SubmitOrder\""));
        assert!(json.contains("\"agent_id\":\"agent-1\""));
        assert!(!json.contains("session_id"));
    }
}
