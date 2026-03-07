//! Secret management API endpoints.
//!
//! Handles encrypted secret storage, retrieval, and deletion for tenants.
//! Secrets are encrypted at rest using the configured vault key.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use tracing::instrument;

use crate::state::ServerState;

/// Check if an error message indicates that the backend is not supported.
fn is_backend_not_supported_error(err: &str) -> bool {
    err.to_ascii_lowercase().contains("not supported")
}

/// PUT /api/tenants/{tenant}/secrets/{key_name} — encrypt and store a secret.
#[instrument(skip_all, fields(tenant, key_name, otel.name = "PUT /api/tenants/{tenant}/secrets/{key_name}"))]
pub(crate) async fn handle_put_secret(
    State(state): State<ServerState>,
    Path((tenant, key_name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured (missing TEMPER_VAULT_KEY)",
        )
            .into_response();
    };

    let body_json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "invalid JSON in put secret request");
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let value = match body_json.get("value").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            tracing::warn!("missing 'value' field in put secret request");
            return (
                StatusCode::BAD_REQUEST,
                "Missing \'value\' field in request body",
            )
                .into_response();
        }
    };

    if value.len() > crate::secrets::vault::MAX_SECRET_VALUE_BYTES {
        tracing::warn!(size = value.len(), "secret value exceeds maximum size");
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "Secret value exceeds maximum size of {} bytes",
                crate::secrets::vault::MAX_SECRET_VALUE_BYTES
            ),
        )
            .into_response();
    }

    // Encrypt the value.
    let (ciphertext, nonce) = match vault.encrypt(value.as_bytes()) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "encryption failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encryption failed: {e}"),
            )
                .into_response();
        }
    };

    // Persist first.
    if let Err(e) = state
        .upsert_secret(&tenant, &key_name, &ciphertext, &nonce)
        .await
    {
        let status = if is_backend_not_supported_error(&e) {
            StatusCode::NOT_IMPLEMENTED
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        tracing::error!(error = %e, "secret persistence failed");
        return (status, format!("Persistence failed: {e}")).into_response();
    }

    // Cache in memory after successful persistence.
    if let Err(e) = vault.cache_secret(&tenant, &key_name, value.to_string()) {
        // Best-effort rollback to keep storage/cache aligned.
        let _ = state.delete_secret(&tenant, &key_name).await;
        tracing::error!(error = %e, "cache update failed after persistence write");
        return (
            StatusCode::CONFLICT,
            format!("Cache update failed after persistence write: {e}"),
        )
            .into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// DELETE /api/tenants/{tenant}/secrets/{key_name} — remove a secret.
#[instrument(skip_all, fields(tenant, key_name, otel.name = "DELETE /api/tenants/{tenant}/secrets/{key_name}"))]
pub(crate) async fn handle_delete_secret(
    State(state): State<ServerState>,
    Path((tenant, key_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured",
        )
            .into_response();
    };

    match state.delete_secret(&tenant, &key_name).await {
        Ok(true) => {
            vault.remove_secret(&tenant, &key_name);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => {
            tracing::warn!("secret not found for deletion");
            vault.remove_secret(&tenant, &key_name);
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            let status = if is_backend_not_supported_error(&e) {
                StatusCode::NOT_IMPLEMENTED
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            tracing::error!(error = %e, "secret deletion failed");
            (status, format!("Delete failed: {e}")).into_response()
        }
    }
}

/// GET /api/tenants/{tenant}/secrets — list secret key names (never values).
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/secrets"))]
pub(crate) async fn handle_list_secrets(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
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
