//! What a caller hears about the store's writes: `/healthz`, and the answer to
//! a move the store could not take.

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::response::odata_error;
use crate::state::ServerState;
use crate::storage::WriteHealth;

/// `/healthz`: whether this server answers, and whether its store takes writes.
///
/// It answers 503 while the store's appends fail, from the first failed append
/// until one lands. Tensorlake's supervisor restarts a process after six failed
/// asks ten seconds apart (`katagami-control`'s `HEALTH_FAILURE_THRESHOLD`), so
/// a store that failed inside this process is started again; nothing else
/// recovers it. A refusal for room answers 200 with the refusal in the body: a
/// restart makes no room (growing the disk does), and while it starts this
/// server serves no reads either.
pub async fn healthz(State(state): State<ServerState>) -> Response {
    let health = state
        .event_journal()
        .map_or(WriteHealth::Writing, |(store, _)| store.write_health());
    let (status, body) = match health {
        WriteHealth::Writing => (StatusCode::OK, serde_json::json!({ "ok": true })),
        WriteHealth::Refusing(error) => (
            StatusCode::OK,
            serde_json::json!({ "ok": true, "writes": format!("refused: {error}") }),
        ),
        WriteHealth::Failing(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({ "ok": false, "writes": format!("failing: {error}") }),
        ),
    };
    (status, axum::Json(body)).into_response()
}

/// The answer to a move the store could not take: 503 `StoreUnavailable` with
/// the store's own words. Retry-After is one second, as for a dispatch that ran
/// out of tries (ADR-0048): each try costs a refusal and no write.
pub(crate) fn store_unavailable(error: &str) -> Response {
    let mut response =
        odata_error(StatusCode::SERVICE_UNAVAILABLE, "StoreUnavailable", error).into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}
