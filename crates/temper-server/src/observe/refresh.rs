//! SSE endpoint for observe UI refresh hints.
//!
//! The frontend subscribes to `/observe/refresh/stream` and re-fetches
//! the relevant REST endpoint when it receives a matching hint.

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::state::ServerState;

/// GET /observe/refresh/stream -- SSE stream of refresh hints.
///
/// Each event has `event: refresh` and `data: {"kind": "Specs"}` (or whichever
/// variant changed). The frontend uses these to selectively re-fetch data.
pub(crate) async fn handle_refresh_stream(
    State(state): State<ServerState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.observe_refresh_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(hint) => {
            let data = serde_json::to_string(&serde_json::json!({"kind": format!("{:?}", hint)}))
                .unwrap_or_default();
            Some(Ok(Event::default().event("refresh").data(data)))
        }
        // Lagged receiver: skip missed events and continue.
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text(""),
    )
}
