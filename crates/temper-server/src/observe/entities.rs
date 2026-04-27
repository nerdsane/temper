//! Entity instance endpoints: list, history, and SSE event stream.

use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use temper_runtime::persistence::EventStore;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::dispatch::extract_tenant;
use crate::entity_actor::{EntityEvent, EntityMsg, EntityResponse};
use crate::state::ServerState;

use super::{EntityInstanceSummary, EventStreamParams};

/// GET /observe/entities -- list active entity instances from the actor registry.
///
/// Returns deduplicated entities with their current state, sorted newest first.
pub(crate) async fn list_entities(
    State(state): State<ServerState>,
) -> Json<Vec<EntityInstanceSummary>> {
    // PG-backed: entity state is tracked in entity_state_cache (updated by actor broadcasts).
    let cache = state.entity_state_cache.read().unwrap(); // ci-ok: infallible lock
    let entities: Vec<EntityInstanceSummary> = cache
        .iter()
        .map(|(key, (current_state, last_updated))| {
            // Keys are "{tenant}:{entity_type}:{entity_id}"
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            EntityInstanceSummary {
                entity_type: parts.get(1).unwrap_or(&"unknown").to_string(),
                entity_id: parts.get(2).unwrap_or(&"unknown").to_string(),
                actor_status: "active".to_string(),
                current_state: Some(current_state.clone()),
                last_updated: Some(last_updated.to_rfc3339()),
            }
        })
        .collect();
    // Sort newest first (by last_updated descending, entities without timestamps go last)
    entities.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));
    Json(entities)
}

/// GET /observe/entities/{entity_type}/{entity_id}/history -- entity event history.
///
/// Returns the full event log for an entity. Checks two sources in order:
/// 1. In-memory actor state (if the actor is currently loaded).
/// 2. Postgres event store (if configured, for inactive entities).
pub(crate) async fn get_entity_history(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((entity_type, entity_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let tenant = extract_tenant(&headers, &state);

    // PG-backed: read current state from entity_state_cache (populated by actor broadcasts).
    let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
    if let Some((current_state, _)) = state.entity_state_cache.read().unwrap().get(&actor_key) {
        let json = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "current_state": current_state,
        });
        return Json(json);
    }

    // Path 2: Query event store directly (for inactive entities).
    if let Some(ref store) = state.event_store {
        let persistence_id = format!("{entity_type}:{entity_id}");
        if let Ok(envelopes) = store.read_events(&persistence_id, 0).await {
            let events: Vec<serde_json::Value> = envelopes
                .iter()
                .filter_map(|env| serde_json::from_value::<EntityEvent>(env.payload.clone()).ok())
                .enumerate()
                .map(|(i, event)| {
                    serde_json::json!({
                        "sequence": i + 1,
                        "action": event.action,
                        "from_state": event.from_status,
                        "to_state": event.to_status,
                        "timestamp": event.timestamp,
                        "params": event.params,
                    })
                })
                .collect();

            return Json(serde_json::json!({
                "entity_type": entity_type,
                "entity_id": entity_id,
                "events": events,
            }));
        }
    }

    // No data sources available.
    Json(serde_json::json!({
        "entity_type": entity_type,
        "entity_id": entity_id,
        "events": [],
    }))
}

/// Format entity events into the history API response shape.
fn format_history_response(
    entity_type: &str,
    entity_id: &str,
    events: &[EntityEvent],
) -> serde_json::Value {
    let formatted: Vec<serde_json::Value> = events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            serde_json::json!({
                "sequence": i + 1,
                "action": e.action,
                "from_state": e.from_status,
                "to_state": e.to_status,
                "timestamp": e.timestamp,
                "params": e.params,
            })
        })
        .collect();

    serde_json::json!({
        "entity_type": entity_type,
        "entity_id": entity_id,
        "events": formatted,
    })
}

// ---------------------------------------------------------------------------
// Phase 2: SSE event stream
// ---------------------------------------------------------------------------

/// GET /observe/events/stream -- Server-Sent Events stream of entity transitions.
///
/// Subscribes to the broadcast channel and streams every `EntityStateChange`
/// as a JSON SSE event. Supports optional `?entity_type=X&entity_id=Y` filters.
pub(crate) async fn handle_event_stream(
    State(state): State<ServerState>,
    Query(params): Query<EventStreamParams>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let filter_type = params.entity_type;
    let filter_id = params.entity_id;

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(change) => {
                // Apply filters.
                if let Some(ref ft) = filter_type
                    && change.entity_type != *ft
                {
                    return None;
                }
                if let Some(ref fi) = filter_id
                    && change.entity_id != *fi
                {
                    return None;
                }
                let data = serde_json::to_string(&change).unwrap_or_default();
                Some(Ok(Event::default().event("state_change").data(data)))
            }
            // Lagged receiver: skip missed events and continue.
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
