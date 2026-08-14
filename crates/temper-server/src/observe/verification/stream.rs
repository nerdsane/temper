//! GET /observe/design-time/stream -- SSE stream of design-time events.

use std::convert::Infallible;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use temper_authz::AuthenticatedRequestContext;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use crate::authz::{observe_tenant_scope, require_authenticated_context, require_observe_auth};
use crate::state::ServerState;

/// GET /observe/design-time/stream -- SSE stream of design-time events.
///
/// Subscribes to the design-time broadcast channel and streams events
/// as they happen (spec loaded, verification started/level/done).
#[instrument(skip_all, fields(otel.name = "GET /observe/design-time/stream"))]
pub(crate) async fn handle_design_time_stream(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_events", "Event")?;
    let filter_tenant = observe_tenant_scope(authenticated).as_str().to_string();
    let rx = state.design_time_tx.subscribe();

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(event) => {
                if event.tenant != filter_tenant {
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
