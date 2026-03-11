//! Entity instance endpoints: list, history, and SSE event stream.

use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use temper_runtime::persistence::EventStore;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::authz::{observe_tenant_scope, require_observe_auth};
use crate::entity_actor::{EntityEvent, EntityMsg, EntityResponse};
use crate::odata::extract_tenant;
use crate::state::ServerState;

use super::{EntityInstanceSummary, EventStreamParams};

/// GET /observe/entities -- list active entity instances from the actor registry.
///
/// Returns deduplicated entities with their current state, sorted newest first.
pub(crate) async fn handle_list_entities(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_entities", "Entity")?;
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let registry = state.actor_registry.read().unwrap(); // ci-ok: infallible lock
    let cache = state.entity_state_cache.read().unwrap(); // ci-ok: infallible lock
    let mut entities: Vec<EntityInstanceSummary> = registry
        .keys()
        .filter_map(|key| {
            // Actor keys are formatted as "{tenant}:{entity_type}:{entity_id}"
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            if let Some(ref scope) = tenant_scope
                && parts.first() != Some(&scope.as_str())
            {
                return None;
            }
            let (current_state, last_updated) = cache
                .get(key.as_str())
                .map(|(s, t)| (Some(s.clone()), Some(t.to_rfc3339())))
                .unwrap_or((None, None));
            Some(EntityInstanceSummary {
                tenant: parts.first().unwrap_or(&"default").to_string(),
                entity_type: parts.get(1).unwrap_or(&"unknown").to_string(),
                entity_id: parts.get(2).unwrap_or(&"unknown").to_string(),
                actor_status: "active".to_string(),
                current_state,
                last_updated,
            })
        })
        .collect();
    // Sort newest first (by last_updated descending, entities without timestamps go last)
    entities.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));
    let total = entities.len();
    Ok(Json(
        serde_json::json!({ "entities": entities, "total": total }),
    ))
}

/// GET /observe/entities/{entity_type}/{entity_id}/history -- entity event history.
///
/// Returns the full event log for an entity. Checks two sources in order:
/// 1. In-memory actor state (if the actor is currently loaded).
/// 2. Postgres event store (if configured, for inactive entities).
pub(crate) async fn handle_get_entity_history(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((entity_type, entity_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_entities", "Entity")?;
    let tenant = extract_tenant(&headers, &state).map_err(|(code, _)| code)?;

    // Path 1: If the actor is loaded, read events from in-memory state.
    let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
    let actor_ref = {
        let registry = state
            .actor_registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        registry.get(&actor_key).cloned()
    };

    if let Some(actor_ref) = actor_ref
        && let Ok(response) = actor_ref
            .ask::<EntityResponse>(EntityMsg::GetState, state.action_dispatch_timeout)
            .await
    {
        let mut json = format_history_response(&entity_type, &entity_id, &response.state.events);
        // Include entity properties from in-memory state.
        if let Some(obj) = json.as_object_mut() {
            obj.insert(
                "current_state".to_string(),
                serde_json::json!(response.state.status),
            );
            obj.insert("fields".to_string(), response.state.fields.clone());
            obj.insert(
                "counters".to_string(),
                serde_json::json!(response.state.counters),
            );
            obj.insert(
                "booleans".to_string(),
                serde_json::json!(response.state.booleans),
            );
            obj.insert("lists".to_string(), serde_json::json!(response.state.lists));
        }
        return Ok(Json(json));
    }

    // Path 2: Query event store directly (for inactive entities).
    if let Some(ref store) = state.event_store {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
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

            return Ok(Json(serde_json::json!({
                "entity_type": entity_type,
                "entity_id": entity_id,
                "events": events,
            })));
        }
    }

    // No data sources available.
    Ok(Json(serde_json::json!({
        "entity_type": entity_type,
        "entity_id": entity_id,
        "events": [],
    })))
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
    headers: HeaderMap,
    Query(params): Query<EventStreamParams>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    require_observe_auth(&state, &headers, "read_events", "Entity")?;
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let rx = state.event_tx.subscribe();
    let filter_type = params.entity_type;
    let filter_id = params.entity_id;
    let filter_tenant = tenant_scope.map(|t| t.as_str().to_string());

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(change) => {
                // Apply tenant filter.
                if let Some(ref ft) = filter_tenant
                    && change.tenant != *ft
                {
                    return None;
                }
                // Apply entity type/id filters.
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

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
