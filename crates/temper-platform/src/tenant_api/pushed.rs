//! `POST /api/os-apps/{name}/install`: install an app's files, pushed by the
//! caller, into this running server through the one installer
//! ([`crate::genesis_install::install_pushed_app`]).
//!
//! Body: `{"files": {"<path in the app>": "<text>" | {"base64": "..."} |
//! {"keep": "<sha256 hex>"}}, "budget_ms": <time each spec's check may
//! take>}`. The app goes into the credential's own tenant; a request cannot
//! name another. Answers 200 with the install, 400 for a wrong request, 422
//! when a spec does not parse or the installer refused it, with why. The
//! install checks at main's level (each spec parses; joint proofs warn), so
//! `budget_ms` bounds no check yet; it is still validated, because the
//! caller (Katagami's apps_build) sends it for the per-spec check. Every answer
//! says how long the push waited for this host's install turn (`waited_ms`).
//!
//! The answer does not wait for Jev's review of the app's changed modules. It
//! names the reviews left running (`result.reviewing`), and
//! `GET /api/os-apps/{name}/review/{version}` says what became of one, to whoever
//! may install the app. `version` is the push's `COMMIT` file, else the bundle's
//! digest. 200 with `{"app", "version", "state"}` unless denied:
//! - `reading`: Jev is still reading.
//! - `done`: and `warnings`, `modules_reviewed`, as an install's answer has them.
//!   `persistence_error`, when present, says the save failed: findings remain
//!   readable in this process until a newer review replaces them or it restarts.
//! - `none`: nothing is kept for that version: no module of it changed, or a
//!   newer review took its place, or a restart stopped it.

use std::collections::BTreeMap;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde::Deserialize;
use temper_authz::AuthenticatedRequestContext;
use temper_server::observe::{DEFAULT_BUDGET_MS, MAX_BUDGET_MS};

use super::auth::{
    PlatformResourceAuthorization, require_authenticated, require_requested_resource_authorization,
};
use super::authorization_error;
use crate::genesis_install::{PushRefused, PushedFile, install_pushed_app};
use crate::module_review::ReviewStatus;
use crate::state::PlatformState;

/// The largest push this server reads. A push is held in memory while it is
/// decoded and checked, and any credential of the tenant can send one before
/// Cedar decides on it, so its size is bounded: by the largest body this
/// server already takes in one request, a blob upload.
const PUSH_BODY_LIMIT_BYTES: usize = 128 * 1024 * 1024;

/// The body of a push.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PushRequest {
    /// Every file of the app, by its path in the app.
    files: BTreeMap<String, PushedFile>,
    /// Time each spec's check may take, in milliseconds.
    #[serde(default)]
    budget_ms: Option<u64>,
}

/// The routes: a push, with the body limit it needs, and what became of its review.
pub(super) fn routes() -> Router<PlatformState> {
    Router::new()
        .route(
            "/os-apps/{name}/install",
            post(install).layer(DefaultBodyLimit::max(PUSH_BODY_LIMIT_BYTES)),
        )
        .route("/os-apps/{name}/review/{version}", get(review))
}

/// Whoever may install the app may read what its installs found. The tenant is the credential's own.
async fn authorize(
    state: &PlatformState,
    authenticated: Option<&AuthenticatedRequestContext>,
    name: &str,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let authenticated = require_authenticated(authenticated).map_err(authorization_error)?;
    let tenant = authenticated.tenant().as_str();
    require_requested_resource_authorization(
        state,
        authenticated,
        PlatformResourceAuthorization {
            action: "install_app",
            resource_type: "App",
            resource_id: name,
            attrs: BTreeMap::from([(
                "targetTenant".to_string(),
                serde_json::Value::String(tenant.to_string()),
            )]),
        },
    )
    .await?;
    Ok(tenant.to_string())
}

async fn review(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path((name, version)): Path<(String, String)>,
) -> impl IntoResponse {
    let tenant = match authorize(&state, authenticated.as_deref(), &name).await {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    let said = |word: &str| serde_json::json!({ "app": name, "version": version, "state": word });
    // Keep a review listed until it is saved, or retain its findings and failure
    // here. A failed save must never turn a completed review into `none`.
    match state.module_reviews.status(&tenant, &name, &version) {
        Some(ReviewStatus::Reading) => return (StatusCode::OK, Json(said("reading"))),
        Some(ReviewStatus::SaveFailed(failure)) => {
            let mut answer = said("done");
            answer["warnings"] = failure.result["warnings"].clone();
            answer["modules_reviewed"] = failure.result["modules_reviewed"].clone();
            answer["persistence_error"] = failure.error.into();
            return (StatusCode::OK, Json(answer));
        }
        None => {}
    }
    let store = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone());
    let kept = match store {
        Some(store) => store.load_module_review(&tenant, &name).await,
        None => Ok(None),
    };
    let row = match kept {
        Ok(row) => row.filter(|row| row.version == version),
        Err(error) => return refusal(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let Some(row) = row else {
        return (StatusCode::OK, Json(said("none")));
    };
    let result: serde_json::Value = match serde_json::from_str(&row.result) {
        Ok(result) => result,
        Err(error) => return refusal(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let mut answer = said("done");
    answer["warnings"] = result["warnings"].clone();
    answer["modules_reviewed"] = result["modules_reviewed"].clone();
    (StatusCode::OK, Json(answer))
}

async fn install(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(name): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let tenant = match authorize(&state, authenticated.as_deref(), &name).await {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    let tenant = tenant.as_str();

    let request: PushRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => return refusal(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let budget_ms = request.budget_ms.unwrap_or(DEFAULT_BUDGET_MS);
    if !(1..=MAX_BUDGET_MS).contains(&budget_ms) {
        return refusal(
            StatusCode::BAD_REQUEST,
            format!("budget_ms must be between 1 and {MAX_BUDGET_MS}"),
        );
    }

    let (waited, pushed) = install_pushed_app(&state, tenant, &name, request.files).await;
    let (status, mut answer) = match pushed {
        Ok(pushed) => (
            StatusCode::OK,
            serde_json::json!({
                "installed": name,
                "digest": pushed.digest,
                "result": pushed.result,
            }),
        ),
        Err(PushRefused::Request(error)) => (StatusCode::BAD_REQUEST, error_body(error)),
        Err(PushRefused::Unparsed(error)) => (StatusCode::UNPROCESSABLE_ENTITY, error_body(error)),
        Err(PushRefused::Install(error)) => (StatusCode::UNPROCESSABLE_ENTITY, error_body(error)),
        Err(PushRefused::Server(error)) => (StatusCode::INTERNAL_SERVER_ERROR, error_body(error)),
    };
    // How long the push waited for this host's install turn, so a slow answer
    // says whether it was the push or the queue.
    answer["waited_ms"] = serde_json::json!(waited.as_millis() as u64);
    (status, Json(answer))
}

fn error_body(error: String) -> serde_json::Value {
    serde_json::json!({ "error": error })
}

fn refusal(status: StatusCode, error: String) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(error_body(error)))
}

#[cfg(test)]
#[path = "pushed_test.rs"]
mod tests;
