//! Secret management API endpoints.
//!
//! Handles encrypted secret storage, retrieval, and deletion for tenants.
//! Secrets are encrypted at rest using the configured vault key.

use std::collections::BTreeMap;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::persistence::SecretPrecondition;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use crate::authz::{
    ResourceAuthorization, require_authenticated_context, require_resource_authorization,
    require_tenant_match,
};
use crate::state::{SecretWriteOutcome, ServerState};

const SECRET_COLLECTION_RESOURCE_ID: &str = "__keys__";

pub(super) struct SecretAuthed(AuthenticatedRequestContext);

impl SecretAuthed {
    fn tenant(&self) -> &TenantId {
        self.0.tenant()
    }
}

impl FromRequestParts<ServerState> for SecretAuthed {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ServerState,
    ) -> Result<Self, Self::Rejection> {
        let Path(params) = Path::<BTreeMap<String, String>>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let Some(tenant) = params.get("tenant") else {
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        };
        let authenticated =
            require_authenticated_context(parts.extensions.get::<AuthenticatedRequestContext>())
                .map_err(IntoResponse::into_response)?;
        require_tenant_match(authenticated, tenant).map_err(IntoResponse::into_response)?;
        let resource_id = params
            .get("key_name")
            .map(String::as_str)
            .unwrap_or(SECRET_COLLECTION_RESOURCE_ID);
        require_resource_authorization(
            state,
            authenticated,
            ResourceAuthorization {
                action: "manage_secrets",
                resource_type: "Secret",
                resource_id,
                resource_attrs: BTreeMap::from([
                    (
                        "tenant".to_string(),
                        serde_json::Value::String(tenant.clone()),
                    ),
                    (
                        "key_name".to_string(),
                        serde_json::Value::String(resource_id.to_string()),
                    ),
                ]),
            },
        )
        .map_err(IntoResponse::into_response)?;
        Ok(Self(authenticated.clone()))
    }
}

/// Check if an error message indicates that the backend is not supported.
fn is_backend_not_supported_error(err: &str) -> bool {
    err.to_ascii_lowercase().contains("not supported")
}

/// The condition a PUT's `If-None-Match` or `If-Match` header puts on the write.
///
/// The agreed API contract in PR #114 restricts conditional PUTs to
/// `If-None-Match: *` and one strong `If-Match` etag:
/// https://github.com/arni-labs/temper/pull/114
/// That contract explicitly chooses 400 for `If-Match: *`, etag lists, weak
/// etags, non-wildcard `If-None-Match`, and both headers together. These checks
/// preserve that settled restriction; supporting existence or version-set
/// predicates requires an owner decision to expand the contract. Never ignore
/// a rejected condition, which would turn the request into an unconditional
/// overwrite.
fn write_precondition(headers: &HeaderMap) -> Result<SecretPrecondition<'_>, &'static str> {
    fn single(headers: &HeaderMap, name: header::HeaderName) -> Result<Option<&str>, ()> {
        let mut values = headers.get_all(name).iter();
        match (values.next(), values.next()) {
            (None, _) => Ok(None),
            (Some(value), None) => value.to_str().map(|v| Some(v.trim())).map_err(|_| ()),
            (Some(_), Some(_)) => Err(()),
        }
    }
    let if_none_match = single(headers, header::IF_NONE_MATCH)
        .map_err(|()| "If-None-Match must be one header holding *")?;
    let if_match = single(headers, header::IF_MATCH)
        .map_err(|()| "If-Match must be one header holding one etag")?;
    match (if_none_match, if_match) {
        (None, None) => Ok(SecretPrecondition::Any),
        (Some(_), Some(_)) => Err("Send If-None-Match or If-Match, not both"),
        (Some("*"), None) => Ok(SecretPrecondition::Absent),
        (Some(_), None) => Err("If-None-Match supports only *"),
        (None, Some(tag)) => tag
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .filter(|id| !id.is_empty() && !id.contains('"'))
            .map(SecretPrecondition::Version)
            .ok_or("If-Match takes one strong etag, as a PUT returned it in ETag"),
    }
}

/// The `ETag` header value for a secret's version id.
fn etag(version_id: &str) -> String {
    format!("\"{version_id}\"")
}

/// PUT /api/tenants/{tenant}/secrets/{key_name} — encrypt and store a secret.
///
/// Answers 204 with the new `ETag`. With `If-None-Match: *` the write happens
/// only if the key does not exist, and with `If-Match: "<etag>"` only if the
/// key's current etag is that one; otherwise 412, which carries no etag.
#[instrument(skip_all, fields(tenant, key_name, otel.name = "PUT /api/tenants/{tenant}/secrets/{key_name}"))]
pub(super) async fn handle_put_secret(
    State(state): State<ServerState>,
    Path((_tenant, key_name)): Path<(String, String)>,
    auth: SecretAuthed,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str();
    let precondition = match write_precondition(&headers) {
        Ok(precondition) => precondition,
        Err(message) => {
            tracing::warn!(reason = message, "unsupported secret write condition");
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
    };
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

    // Persist the encrypted secret; the store checks the condition in the
    // same statement that writes.
    let version_id = match state
        .write_secret(tenant, &key_name, &ciphertext, &nonce, precondition)
        .await
    {
        Ok(SecretWriteOutcome::Written { version_id }) => version_id,
        Ok(SecretWriteOutcome::PreconditionFailed) => {
            tracing::info!("secret write precondition failed");
            return (
                StatusCode::PRECONDITION_FAILED,
                "The secret does not meet the If-None-Match or If-Match condition",
            )
                .into_response();
        }
        Err(e) => {
            let status =
                if precondition != SecretPrecondition::Any && is_backend_not_supported_error(&e) {
                    StatusCode::NOT_IMPLEMENTED
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
            tracing::error!(error = %e, "secret persistence failed");
            return (status, format!("Persistence failed: {e}")).into_response();
        }
    };

    // Cache in memory after successful persistence.
    vault.cache_secret(tenant, &key_name, value.to_string());

    let mut response = StatusCode::NO_CONTENT.into_response();
    match HeaderValue::from_str(&etag(&version_id)) {
        Ok(value) => {
            response.headers_mut().insert(header::ETAG, value);
        }
        // A version id is 32 hex digits, so this cannot happen; if it did, the
        // write has still happened and the caller can read the etag back.
        Err(e) => tracing::error!(error = %e, "secret version id is not a header value"),
    }
    response
}

/// DELETE /api/tenants/{tenant}/secrets/{key_name} — remove a secret.
#[instrument(skip_all, fields(tenant, key_name, otel.name = "DELETE /api/tenants/{tenant}/secrets/{key_name}"))]
pub(super) async fn handle_delete_secret(
    State(state): State<ServerState>,
    Path((_tenant, key_name)): Path<(String, String)>,
    auth: SecretAuthed,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str();
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured",
        )
            .into_response();
    };

    match state.delete_secret(tenant, &key_name).await {
        Ok(true) => {
            vault.remove_secret(tenant, &key_name);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => {
            tracing::warn!("secret not found for deletion");
            vault.remove_secret(tenant, &key_name);
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
///
/// `keys` names every secret the tenant can read, platform secrets included.
/// `etags` maps each stored tenant secret to its current `ETag`, for a later
/// `If-Match`.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/secrets"))]
pub(super) async fn handle_list_secrets(
    State(state): State<ServerState>,
    Path(_tenant): Path<String>,
    auth: SecretAuthed,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str();
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "Secrets vault not configured"})),
        )
            .into_response();
    };

    let keys = vault.list_keys(tenant);
    let etags: BTreeMap<String, String> = match state.secret_versions(tenant).await {
        Ok(versions) => versions
            .into_iter()
            .map(|(key_name, version_id)| (key_name, etag(&version_id)))
            .collect(),
        Err(e) => {
            tracing::error!(error = %e, "secret version listing failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": format!("Listing failed: {e}")})),
            )
                .into_response();
        }
    };
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"keys": keys, "etags": etags})),
    )
        .into_response()
}
