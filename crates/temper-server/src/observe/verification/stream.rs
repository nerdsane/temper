//! GET /observe/design-time/stream -- SSE stream of design-time events.

use std::convert::Infallible;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use crate::authz::{observe_tenant_scope, require_observe_auth};
use crate::state::ServerState;

/// GET /observe/design-time/stream -- SSE stream of design-time events.
///
/// Subscribes to the design-time broadcast channel and streams events
/// as they happen (spec loaded, verification started/level/done).
#[instrument(skip_all, fields(otel.name = "GET /observe/design-time/stream"))]
pub(crate) async fn handle_design_time_stream(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    require_observe_auth(&state, &headers, "read_events", "Event")?;
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let filter_tenant = tenant_scope.map(|t| t.as_str().to_string());
    let rx = state.design_time_tx.subscribe();

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(event) => {
                if let Some(ref tenant) = filter_tenant
                    && event.tenant != *tenant
                {
                    return None;
                }
                let data = serde_json::to_string(&event).unwrap_or_default();
                Some(Ok(Event::default().event("design_time").data(data)))
            }
            // Lagged receiver: skip missed events and continue.
            Err(_) => None,
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
