//! Answers wait for the database's Turso Cloud copy (Turso Sync).
//!
//! With `TURSO_SYNC_URL` set, the database stays a local file and every
//! request that can change it is answered only after its changes reach the
//! remote copy. Requests that finish together share one push. A background
//! loop also pushes changes no request waits for (timers, reactions), trims
//! the change log the remote has confirmed, keeps the local write-ahead log
//! short, and checks a file the start could not check. While writes cannot
//! be confirmed, requests that can change data are refused before they run
//! and reads are served as usual.

use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use temper_server::odata::IDEMPOTENCY_KEY;
use temper_store_turso::{CopyState, SyncCopy, TursoEventStore};

/// How long a request that can change data may take, its push included,
/// before its change is reported as not confirmed.
const ANSWER_BUDGET: Duration = Duration::from_secs(10);

/// The least time left for the push of a slow request.
const MIN_PUSH_BUDGET: Duration = Duration::from_secs(1);

/// Default period of the background push.
const DEFAULT_UPKEEP_INTERVAL: Duration = Duration::from_secs(1);

/// The 1 s ceiling lets the background push and the checkpoint run once each
/// tick. Requests need no share of it: a write that waits goes before the
/// trim's next batch (`TursoEventStore::trim_change_log`).
const TRIM_BUDGET: Duration = Duration::from_secs(1);

/// Checkpoint the local log once it passes this size. Turso Sync turns the
/// engine's automatic checkpoints off.
const MAX_WAL_BYTES: u64 = 16 * 1024 * 1024;

/// Push attempts timing out in a row before the server exits. The sync
/// engine sends its requests one at a time, so one the remote never answers
/// holds every later push until its connection fails; the supervisor's
/// restart starts a fresh one, and the start checks the file again.
const TIMEOUTS_BEFORE_RESTART: u32 = 6;

/// The server's router: `routes` with, when the store's database has a copy,
/// changes answered only after they reach it, and one span per request
/// around all of it. The span goes on last, so a request's push, and the
/// answer it ends in, are part of that request's trace.
pub(super) fn serve_router(routes: Router, store: Option<TursoEventStore>) -> Router {
    temper_server::with_request_span(answer_after_copy(routes, store))
}

/// With a store whose database has a copy, answer changes only after they
/// reach it and keep it current.
fn answer_after_copy(routes: Router, store: Option<TursoEventStore>) -> Router {
    let Some((store, copy)) =
        store.and_then(|store| store.sync_copy().cloned().map(|copy| (store, copy)))
    else {
        return routes;
    };
    spawn_upkeep(store, copy.clone());
    routes.layer(axum::middleware::from_fn_with_state(
        copy,
        push_before_answering,
    ))
}

/// Refuse a request that can change data while writes cannot be confirmed;
/// otherwise run it and push what it committed before answering.
async fn push_before_answering(
    State(copy): State<SyncCopy>,
    request: Request,
    next: Next,
) -> Response {
    if reads_only(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    if let Some(refusal) = refusal(&copy.state()) {
        return refusal;
    }
    let started = Instant::now();
    let response = next.run(request).await;
    let budget = ANSWER_BUDGET
        .saturating_sub(started.elapsed())
        .max(MIN_PUSH_BUDGET);
    match copy.push_committed_within(budget).await {
        Ok(()) => response,
        // The request ran, so whatever the copy's state now, the change may
        // have been saved here: never answer as if nothing happened.
        Err(error) => {
            tracing::error!(%error, "turso.sync: change not copied off this server; not confirming it");
            not_confirmed(response.headers().get(IDEMPOTENCY_KEY))
        }
    }
}

/// The answer to a request that ran but whose change could not be copied
/// off this server. It names the key the request ran with, when it has one,
/// so the caller can send it again as the same write.
fn not_confirmed(key: Option<&HeaderValue>) -> Response {
    let key = key.and_then(|value| Some((value, value.to_str().ok()?)));
    let mut error = serde_json::json!({
        "code": "NotConfirmed",
        "message": "Not confirmed: the change could not be copied to this app's database copy. \
                    It may still have been saved here; read it back before trying again.",
    });
    if let Some((_, text)) = key {
        error["message"] = "Not confirmed: the change could not be copied to this app's database \
                            copy. It may still have been saved here. Send it again with its \
                            Idempotency-Key (idempotencyKey) and it is applied at most once."
            .into();
        error["idempotencyKey"] = text.into();
    }
    let mut answer = (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({ "error": error })),
    )
        .into_response();
    if let Some((value, _)) = key {
        answer.headers_mut().insert(IDEMPOTENCY_KEY, value.clone());
    }
    answer
}

/// The answer to a request that can change data in `state`, if it is refused.
fn refusal(state: &CopyState) -> Option<Response> {
    let (code, message) = match state {
        CopyState::Copying => return None,
        CopyState::Unchecked => (
            "SavingPaused",
            "Saving is paused until this app's database copy can be reached. Reading still works.",
        ),
        CopyState::Behind => (
            "SavingPaused",
            "Saving is paused while this app restores its database from its copy. \
             Try again in a minute.",
        ),
        CopyState::Stopped(_) => (
            "SavingStopped",
            "Saving is stopped: this app's database copy is missing a change, so nothing new \
             can be confirmed until an operator rebuilds the copy. Reading still works.",
        ),
    };
    Some(unavailable(code, message))
}

fn unavailable(code: &str, message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// Requests that cannot change anything: reads of Temper's own routes. A
/// read of an app's HTTP endpoint or webhook can run actions, so it waits too.
fn reads_only(method: &Method, path: &str) -> bool {
    // A question to Cedar is a read sent as a POST: it decides on the snapshot
    // its caller sends and writes nothing (temper-server's api/authorize.rs).
    if *method == Method::POST && path == "/api/authorize" {
        return true;
    }
    let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    let own_route = ["/tdata", "/observe", "/api", "/genesis"]
        .iter()
        .any(|prefix| {
            path.strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
        || matches!(
            path,
            "/healthz" | "/version" | "/temper-client.js" | "/static/temper-client.js"
        );
    safe && own_route
}

/// Push changes no request waited for, check a file the start could not
/// check, trim the change log, checkpoint the local log, and restart when
/// only a restart helps.
fn spawn_upkeep(store: TursoEventStore, copy: SyncCopy) {
    let interval = std::env::var("TEMPER_TURSO_SYNC_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_UPKEEP_INTERVAL);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            copy.record_state();
            if let Err(error) = copy.recheck().await {
                tracing::debug!(%error, "turso.sync remote still unreachable");
            }
            if copy.state() == CopyState::Behind {
                tracing::error!(
                    "turso.sync the file is behind its remote copy; exiting so the restart restores it"
                );
                std::process::exit(75);
            }
            if copy.state() != CopyState::Copying {
                continue;
            }
            if let Err(error) = copy.push_committed().await {
                tracing::warn!(%error, "turso.sync background push failed");
            }
            if copy.pushes_timed_out_in_a_row() >= TIMEOUTS_BEFORE_RESTART {
                tracing::error!(
                    timeouts = copy.pushes_timed_out_in_a_row(),
                    "turso.sync pushes keep timing out; exiting so the restart opens a fresh connection"
                );
                std::process::exit(75);
            }
            if let Err(error) = store.trim_change_log(TRIM_BUDGET).await {
                tracing::warn!(%error, "turso.sync could not trim the change log; the next tick will");
            }
            if let Err(error) = copy.checkpoint_if_wal_over(MAX_WAL_BYTES).await {
                tracing::warn!(%error, "turso.sync checkpoint failed");
            }
        }
    });
}

#[cfg(test)]
mod tests;
