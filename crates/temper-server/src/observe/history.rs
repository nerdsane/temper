//! A record's history: its journaled trail, read whole or newest first.

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::persistence::PersistenceEnvelope;

use crate::authz::{require_authenticated_context, require_observe_auth};
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::entity_actor::{EntityEvent, EntityMsg, EntityResponse, EntityState};
use crate::state::ServerState;

/// GET /observe/entities/{entity_type}/{entity_id}/history -- entity event history.
///
/// `events` is the record's trail in ascending sequence order, each event at
/// its stored sequence number. With an event journal it is always read from
/// the journal, whether or not the actor is loaded: a loaded actor holds only
/// a bounded tail of its events. A journal read failure answers 503 rather
/// than a partial trail. Only a server without a journal answers `events`
/// from the loaded actor's memory.
///
/// Paging, newest first:
/// - no parameters: the whole trail from sequence 1;
/// - `?limit=N`: the newest N events;
/// - `?limit=N&before=S`: the N events just below stored sequence S.
///
/// `next_before` is the lowest sequence in the page when older events exist,
/// and null when the page reaches the start of the trail; following it until
/// it is null reads every event once. A `limit` below 1 or a parameter that
/// is not a whole number answers 400. A page reads only its own rows from the
/// journal, plus one to learn whether older events exist.
///
/// When the actor is loaded the response also carries its live
/// `current_state`, `fields`, `counters`, `booleans` and `lists`.
pub(crate) async fn handle_get_entity_history(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path((entity_type, entity_id)): Path<(String, String)>,
    Query(params): Query<EntityHistoryParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read", &entity_type)?;
    let page = HistoryPage::parse(&params)?;
    let tenant = authenticated.tenant().clone();
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");

    // Live state first: the journal is written before memory changes, so a
    // journal read that follows holds at least every event the state reflects.
    let loaded = loaded_entity_state(&state, &persistence_id).await;
    let (events, next_before) = match state.event_journal() {
        Some((store, _backend)) => {
            let envelopes = store
                .read_events_page(&persistence_id, page.before, page.rows_to_read())
                .await
                .map_err(|error| {
                    tracing::warn!(
                        persistence_id = %persistence_id,
                        error = %error,
                        "entity history journal read failed"
                    );
                    StatusCode::SERVICE_UNAVAILABLE
                })?;
            let (envelopes, next_before) = page.split(envelopes, |envelope| envelope.sequence_nr);
            (
                journal_history_events(&persistence_id, &envelopes),
                next_before,
            )
        }
        None => {
            let held = loaded
                .as_ref()
                .map(|entity| memory_events(entity, page.before))
                .unwrap_or_default();
            let (held, next_before) = page.split(held, |(sequence, _)| *sequence);
            let events: Vec<serde_json::Value> = held
                .into_iter()
                .map(|(sequence, event)| history_event_json(sequence, event))
                .collect();
            (events, next_before)
        }
    };

    let mut json = serde_json::json!({
        "entity_type": entity_type,
        "entity_id": entity_id,
        "events": events,
        "next_before": next_before,
    });
    if let (Some(entity), Some(obj)) = (loaded, json.as_object_mut()) {
        let mut fields = entity.fields;
        hydrate_blob_refs_for_tenant(&state, &tenant, &mut fields).await;
        obj.insert(
            "current_state".to_string(),
            serde_json::json!(entity.status),
        );
        obj.insert("fields".to_string(), fields);
        obj.insert("counters".to_string(), serde_json::json!(entity.counters));
        obj.insert("booleans".to_string(), serde_json::json!(entity.booleans));
        obj.insert("lists".to_string(), serde_json::json!(entity.lists));
    }
    Ok(Json(json))
}

/// Query parameters of the history route, as sent. [`HistoryPage::parse`]
/// validates them.
#[derive(Debug, Deserialize)]
pub(crate) struct EntityHistoryParams {
    /// How many of the newest events (below `before`) to return.
    pub limit: Option<String>,
    /// Exclusive upper bound on the stored sequence of the page's events.
    pub before: Option<String>,
}

/// Which part of a record's trail one history read returns.
#[derive(Debug, Clone, Copy)]
struct HistoryPage {
    /// Only events below this stored sequence; `None` is no bound.
    before: Option<u64>,
    /// At most this many of the newest such events; `None` is no bound.
    limit: Option<usize>,
}

impl HistoryPage {
    /// `limit` must be a whole number of at least 1 and `before` a whole
    /// number; anything else is a 400.
    fn parse(params: &EntityHistoryParams) -> Result<Self, StatusCode> {
        let limit = params
            .limit
            .as_deref()
            .map(|raw| {
                raw.parse::<usize>()
                    .ok()
                    .filter(|limit| *limit >= 1)
                    .ok_or(StatusCode::BAD_REQUEST)
            })
            .transpose()?;
        let before = params
            .before
            .as_deref()
            .map(|raw| raw.parse::<u64>().map_err(|_| StatusCode::BAD_REQUEST))
            .transpose()?;
        Ok(Self { before, limit })
    }

    /// Rows to read: one more than the page, to learn whether older ones exist.
    fn rows_to_read(self) -> usize {
        self.limit
            .map_or(usize::MAX, |limit| limit.saturating_add(1))
    }

    /// Keep the newest `limit` of `rows`, which are ascending and at most
    /// [`Self::rows_to_read`] long. The second value is the lowest sequence
    /// kept when an older row was read, else `None`.
    fn split<T>(self, mut rows: Vec<T>, sequence: impl Fn(&T) -> u64) -> (Vec<T>, Option<u64>) {
        match self.limit {
            Some(limit) if rows.len() > limit => {
                let page = rows.split_off(rows.len() - limit);
                let next_before = page.first().map(sequence);
                (page, next_before)
            }
            _ => (rows, None),
        }
    }
}

/// The state of the record's actor if it is loaded. Never loads one.
async fn loaded_entity_state(state: &ServerState, actor_key: &str) -> Option<EntityState> {
    let actor_ref = {
        let registry = state
            .actor_registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        registry.get(actor_key).cloned()
    }?;
    actor_ref
        .ask::<EntityResponse>(EntityMsg::GetState, state.action_dispatch_timeout)
        .await
        .ok()
        .map(|response| response.state)
}

/// History rows for journal envelopes, at their stored sequence numbers.
fn journal_history_events(
    persistence_id: &str,
    envelopes: &[PersistenceEnvelope],
) -> Vec<serde_json::Value> {
    envelopes
        .iter()
        .filter_map(|envelope| {
            match serde_json::from_value::<EntityEvent>(envelope.payload.clone()) {
                Ok(event) => Some(history_event_json(envelope.sequence_nr, &event)),
                Err(error) => {
                    tracing::warn!(
                        persistence_id,
                        sequence = envelope.sequence_nr,
                        error = %error,
                        "journal event is not an entity event; left out of history"
                    );
                    None
                }
            }
        })
        .collect()
}

/// The events a loaded actor holds below `before`, with their sequences, for
/// a server without a journal. They are the newest events, so they end at the
/// actor's sequence.
fn memory_events(entity: &EntityState, before: Option<u64>) -> Vec<(u64, &EntityEvent)> {
    let first = entity
        .event_sequence()
        .saturating_sub(entity.events.len() as u64)
        + 1;
    (first..)
        .zip(&entity.events)
        .filter(|(sequence, _)| before.is_none_or(|before| *sequence < before))
        .collect()
}

/// One history row. The only place an event's history JSON is built.
fn history_event_json(sequence: u64, event: &EntityEvent) -> serde_json::Value {
    serde_json::json!({
        "sequence": sequence,
        "action": event.action,
        "from_state": event.from_status,
        "to_state": event.to_status,
        "timestamp": event.timestamp,
        "via": event.origin.via,
        "by": event.origin.by,
        "for": event.origin.acting_for,
        "agent_type": event.origin.agent_type,
        "label": event.origin.label,
        "params": event.params,
    })
}
