//! Inbound webhook receiver for external callbacks.
//!
//! Handles GET/POST requests to `/webhooks/{tenant}/{*path}` and dispatches
//! entity actions based on webhook declarations in IOA specs. This enables
//! OAuth2 callbacks, payment gateway notifications, and other external system
//! integrations to trigger entity state transitions.

use std::collections::BTreeMap;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;

use tracing::instrument;

use crate::aws_sigv4::{hex_encode, hmac_sha256};
use crate::request_context::AgentContext;
use crate::secrets::template::resolve_secret_templates;
use crate::state::ServerState;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Webhook;

const WEBHOOK_BODY_BUDGET_BYTES: usize = 64 * 1024;

/// Handle an inbound webhook request.
///
/// Route: `GET|POST /webhooks/{tenant}/{*path}`
///
/// The handler looks up the webhook configuration from the tenant's spec
/// registry, validates the HTTP method, extracts the entity ID and action
/// parameters, then dispatches the configured action to the target entity.
#[instrument(skip_all, fields(
    otel.name = %format_args!("{} /webhooks/{}/{}", method, tenant_str, webhook_path),
    tenant = %tenant_str,
    webhook_path = %webhook_path,
    http.method = %method,
))]
pub async fn handle_webhook(
    method: Method,
    State(state): State<ServerState>,
    Path((tenant_str, webhook_path)): Path<(String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
    request: Request<Body>,
) -> impl IntoResponse {
    let tenant = TenantId::new(&tenant_str);

    // Look up webhook config from registry.
    let lookup = find_webhook(&state, &tenant, &webhook_path);

    let Some((entity_type, webhook)) = lookup else {
        tracing::warn!(path = %webhook_path, "no webhook registered at path");
        return (
            StatusCode::NOT_FOUND,
            format!("No webhook registered at path '{webhook_path}' for tenant '{tenant_str}'"),
        );
    };

    // Validate HTTP method.
    let expected_method = webhook.method.to_uppercase();
    if method.as_str() != expected_method {
        tracing::warn!(expected = %expected_method, actual = %method, "webhook method mismatch");
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            format!(
                "Webhook '{}' expects {} but received {}",
                webhook.name,
                expected_method,
                method.as_str()
            ),
        );
    }

    // Extract entity ID from the configured source.
    let entity_id = {
        let param_name = webhook.entity_param.as_deref().unwrap_or("entity_id");
        query.get(param_name).cloned()
    };

    let Some(entity_id) = entity_id else {
        let param_name = webhook.entity_param.as_deref().unwrap_or("entity_id");
        tracing::warn!(param = %param_name, "missing entity ID in webhook request");
        return (
            StatusCode::BAD_REQUEST,
            format!("Missing entity ID: expected query parameter '{param_name}'"),
        );
    };

    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    let headers = request.headers().clone();
    let body = match to_bytes(request.into_body(), WEBHOOK_BODY_BUDGET_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("Webhook body exceeds {WEBHOOK_BODY_BUDGET_BYTES} bytes"),
            );
        }
    };

    let security_ctx = match admit_webhook(
        &state,
        &tenant,
        &webhook,
        &method,
        &path_and_query,
        &headers,
        &body,
    ) {
        Ok(ctx) => ctx,
        Err(status) => {
            return (status, "Webhook admission denied".to_string());
        }
    };

    // Extract action parameters from the configured extraction map.
    let mut params = serde_json::Map::new();
    for (param_name, source) in &webhook.extract {
        if let Some(value) = extract_param(source, &query) {
            params.insert(param_name.clone(), serde_json::Value::String(value));
        }
    }

    let action = &webhook.action;
    let mut resource_attrs = BTreeMap::new();
    resource_attrs.insert(
        "id".to_string(),
        serde_json::Value::String(entity_id.clone()),
    );
    if let Err(denial) = state.authorize_with_context(
        &security_ctx,
        action,
        &entity_type,
        &resource_attrs,
        tenant.as_str(),
    ) {
        tracing::warn!(reason = %denial, webhook = %webhook.name, "webhook action denied");
        return (
            StatusCode::FORBIDDEN,
            format!("Webhook action denied: {denial}"),
        );
    }

    let agent_ctx = AgentContext {
        security_ctx: Some(security_ctx),
        agent_id: Some(format!("webhook:{}", webhook.name)),
        session_id: None,
        agent_type: Some("webhook".to_string()),
        intent: None,
        ..AgentContext::default()
    };

    match state
        .dispatch_tenant_action(
            &tenant,
            &entity_type,
            &entity_id,
            action,
            serde_json::Value::Object(params),
            &agent_ctx,
        )
        .await
    {
        Ok(response) => {
            let body = serde_json::to_string(&response).unwrap_or_default();
            (StatusCode::OK, body)
        }
        Err(e) => {
            tracing::error!(error = %e, "webhook action dispatch failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Action dispatch failed: {e}"),
            )
        }
    }
}

/// Class B admission: HMAC is required and fail-closed.
fn admit_webhook(
    state: &ServerState,
    tenant: &TenantId,
    webhook: &Webhook,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<SecurityContext, StatusCode> {
    let Some(secret_template) = webhook.hmac_secret.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(webhook = %webhook.name, "webhook missing hmac_secret");
        return Err(StatusCode::UNAUTHORIZED);
    };
    let Some(header_name) = webhook.hmac_header.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(webhook = %webhook.name, "webhook missing hmac_header");
        return Err(StatusCode::UNAUTHORIZED);
    };
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!(webhook = %webhook.name, "webhook HMAC required but no secrets vault");
        return Err(StatusCode::UNAUTHORIZED);
    };
    let mut templates = BTreeMap::new();
    templates.insert("hmac".to_string(), secret_template.to_string());
    let resolved = resolve_secret_templates(&templates, vault, tenant.as_str());
    let secret = resolved.get("hmac").cloned().unwrap_or_default();
    if secret.is_empty() || secret.contains("{secret:") {
        tracing::warn!(webhook = %webhook.name, "webhook HMAC secret did not resolve");
        return Err(StatusCode::UNAUTHORIZED);
    }
    let Some(provided) = headers
        .get(header_name)
        .and_then(|value| value.to_str().ok())
    else {
        tracing::warn!(webhook = %webhook.name, header = %header_name, "missing HMAC header");
        return Err(StatusCode::UNAUTHORIZED);
    };
    let mut payload =
        Vec::with_capacity(method.as_str().len() + path_and_query.len() + body.len() + 2);
    payload.extend_from_slice(method.as_str().as_bytes());
    payload.push(b'\n');
    payload.extend_from_slice(path_and_query.as_bytes());
    payload.push(b'\n');
    payload.extend_from_slice(body);
    if !hmac_hex_matches(secret.as_bytes(), &payload, provided) {
        tracing::warn!(webhook = %webhook.name, "HMAC mismatch");
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(SecurityContext::from_resolved_identity(
        &format!("webhook:{}", webhook.name),
        "webhook",
        None,
    ))
}

fn hmac_hex_matches(secret: &[u8], payload: &[u8], provided: &str) -> bool {
    let expected = hex_encode(&hmac_sha256(secret, payload));
    let provided = provided
        .strip_prefix("sha256=")
        .unwrap_or(provided)
        .to_ascii_lowercase();
    if expected.len() != provided.len() {
        return false;
    }
    expected
        .as_bytes()
        .iter()
        .zip(provided.as_bytes())
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

/// Find a webhook matching (tenant, path) in the registry.
///
/// Checks the pre-indexed `webhook_routes` map first, then falls back to
/// iterating all entity types and their webhook declarations.
fn find_webhook(state: &ServerState, tenant: &TenantId, path: &str) -> Option<(String, Webhook)> {
    let registry = state.registry.read().ok()?;
    let tenant_config = registry.get_tenant(tenant)?;
    // Check pre-indexed webhook_routes first (O(log n) lookup).
    if let Some((entity_type, wh)) = tenant_config.webhook_routes.get(path) {
        return Some((entity_type.clone(), wh.clone()));
    }
    // Fallback: iterate all entity types and their webhooks.
    for (entity_type, spec) in &tenant_config.entities {
        for wh in &spec.automaton.webhooks {
            if wh.path == path {
                return Some((entity_type.clone(), wh.clone()));
            }
        }
    }
    None
}

/// Extract a parameter value from the configured source.
///
/// Supported source formats:
/// - `query.KEY` — extract from URL query string
fn extract_param(source: &str, query: &BTreeMap<String, String>) -> Option<String> {
    if let Some(key) = source.strip_prefix("query.") {
        return query.get(key).cloned();
    }
    // Bare key — also try query string.
    query.get(source).cloned()
}

#[cfg(test)]
#[path = "receiver_test.rs"]
mod tests;
