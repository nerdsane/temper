//! Management API routes (mutations).
//!
//! These endpoints handle spec loading, WASM module management, and evolution
//! decisions.  They are separated from the read-only `/observe` router so that
//! observe stays purely observational.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};

use crate::state::ServerState;

/// Build the management API router (mounted at /api).
///
/// Route structure:
/// - POST   /api/specs/load-dir                        → load specs from directory
/// - POST   /api/specs/load-inline                     → load specs from inline payload
/// - POST   /api/wasm/modules/{module_name}            → upload WASM module
/// - DELETE /api/wasm/modules/{module_name}             → delete WASM module
/// - POST   /api/evolution/records/{id}/decide          → developer decision on record
/// - POST   /api/evolution/trajectories/unmet           → report unmet user intent
/// - POST   /api/evolution/sentinel/check               → trigger sentinel health check
pub fn build_api_router() -> Router<ServerState> {
    Router::new()
        .route(
            "/specs/load-dir",
            post(crate::observe::specs::handle_load_dir),
        )
        .route(
            "/specs/load-inline",
            post(crate::observe::specs::handle_load_inline),
        )
        .route(
            "/wasm/modules/{module_name}",
            post(crate::observe::wasm::upload_wasm_module)
                .delete(crate::observe::wasm::delete_wasm_module),
        )
        .route(
            "/evolution/records/{id}/decide",
            post(crate::observe::evolution::handle_decide),
        )
        .route(
            "/evolution/trajectories/unmet",
            post(crate::observe::evolution::handle_unmet_intent),
        )
        .route(
            "/evolution/sentinel/check",
            post(crate::observe::evolution::handle_sentinel_check),
        )
        .route(
            "/tenants/{tenant}/secrets/{key_name}",
            put(handle_put_secret).delete(handle_delete_secret),
        )
        .route("/tenants/{tenant}/secrets", get(handle_list_secrets))
}

/// PUT /api/tenants/{tenant}/secrets/{key_name} — encrypt and store a secret.
async fn handle_put_secret(
    State(state): State<ServerState>,
    Path((tenant, key_name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let Some(vault) = state.secrets_vault.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured (missing TEMPER_VAULT_KEY)",
        )
            .into_response();
    };

    let body_json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let value = match body_json.get("value").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "Missing \'value\' field in request body",
            )
                .into_response();
        }
    };

    if value.len() > crate::secrets_vault::MAX_SECRET_VALUE_BYTES {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "Secret value exceeds maximum size of {} bytes",
                crate::secrets_vault::MAX_SECRET_VALUE_BYTES
            ),
        )
            .into_response();
    }

    // Encrypt the value.
    let (ciphertext, nonce) = match vault.encrypt(value.as_bytes()) {
        Ok(pair) => pair,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encryption failed: {e}"),
            )
                .into_response();
        }
    };

    // Cache in memory.
    if let Err(e) = vault.cache_secret(&tenant, &key_name, value.to_string()) {
        return (StatusCode::CONFLICT, e).into_response();
    }

    // Persist to DB.
    if let Err(e) = state
        .upsert_secret(&tenant, &key_name, &ciphertext, &nonce)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Persistence failed: {e}"),
        )
            .into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// DELETE /api/tenants/{tenant}/secrets/{key_name} — remove a secret.
async fn handle_delete_secret(
    State(state): State<ServerState>,
    Path((tenant, key_name)): Path<(String, String)>,
) -> impl IntoResponse {
    let Some(vault) = state.secrets_vault.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured",
        )
            .into_response();
    };

    vault.remove_secret(&tenant, &key_name);

    match state.delete_secret(&tenant, &key_name).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Delete failed: {e}"),
        )
            .into_response(),
    }
}

/// GET /api/tenants/{tenant}/secrets — list secret key names (never values).
async fn handle_list_secrets(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
) -> impl IntoResponse {
    let Some(vault) = state.secrets_vault.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "Secrets vault not configured"})),
        )
            .into_response();
    };

    let keys = vault.list_keys(&tenant);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"keys": keys})),
    )
        .into_response()
}
