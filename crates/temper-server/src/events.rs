//! Server-Sent Events (SSE) for entity state change subscriptions.
//!
//! Provides a `/tdata/$events` endpoint that streams real-time entity
//! state transitions to connected clients via SSE.

use std::convert::Infallible;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use tracing::instrument;

use crate::authz::{observe_tenant_scope, require_observe_auth};
use crate::state::ServerState;

/// A notification emitted when an entity transitions to a new state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityStateChange {
    /// Monotonic per-entity event sequence.
    #[serde(default)]
    pub seq: u64,
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
#[instrument(skip_all, fields(otel.name = "GET /tdata/$events"))]
pub async fn handle_events(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    require_observe_auth(&state, &headers, "read_events", "Entity")?;
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let filter_tenant = tenant_scope.map(|t| t.as_str().to_string());
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(change) => {
                // Enforce tenant scope: only emit events for the scoped tenant.
                if let Some(ref tenant) = filter_tenant
                    && change.tenant != *tenant
                {
                    return None;
                }
                let data = serde_json::to_string(&change).unwrap_or_default();
                Some(Ok(Event::default().event("state_change").data(data)))
            }
            // Lagged receiver: skip missed events and continue
            Err(_) => None,
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_state_change_serializes() {
        let change = EntityStateChange {
            seq: 1,
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
