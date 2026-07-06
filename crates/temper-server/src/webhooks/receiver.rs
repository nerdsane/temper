//! Inbound webhook receiver for external callbacks.
//!
//! Handles GET/POST requests to `/webhooks/{tenant}/{*path}` and dispatches
//! entity actions based on webhook declarations in IOA specs. This enables
//! OAuth2 callbacks, payment gateway notifications, and other external system
//! integrations to trigger entity state transitions.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;

use tracing::instrument;

use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Webhook;

use crate::authz::{DenialInput, record_authz_denial};
use crate::request_context::AgentContext;
use crate::secrets::resolve_secret_templates;
use crate::state::ServerState;

/// Default header carrying the webhook HMAC signature when the spec's
/// `hmac_header` is unset.
const DEFAULT_SIGNATURE_HEADER: &str = "X-Temper-Signature";

/// Handle an inbound webhook request.
///
/// Route: `GET|POST /webhooks/{tenant}/{*path}`
///
/// The handler looks up the webhook configuration from the tenant's spec
/// registry, validates the HTTP method, then applies the two gates that guard
/// every other write path (ADR-0156):
///
/// 1. **Authenticity** — when the webhook declares `hmac_secret`, the request
///    must carry a valid `HMAC-SHA256(secret, raw_body)` signature or it is
///    rejected `401`.
/// 2. **Authorization** — a restricted `webhook:{name}` principal is built and
///    the configured action is authorized through the same Cedar gate as the
///    OData write path; a denied request is rejected `403`.
///
/// Only after both gates pass is the action dispatched.
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
    headers: HeaderMap,
    body: Bytes,
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

    // Gate 1 — authenticity (ADR-0156). Verify the HMAC signature over the raw
    // body before touching entity state. `authenticated` records whether a
    // declared secret verified the request; Cedar policies can require it.
    let authenticated = match verify_webhook_signature(&state, &tenant, &webhook, &headers, &body) {
        Ok(authenticated) => authenticated,
        Err((status, message)) => {
            tracing::warn!(webhook = %webhook.name, %status, "webhook signature rejected: {message}");
            return (status, message);
        }
    };

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

    // Extract action parameters from the configured extraction map.
    let mut params = serde_json::Map::new();
    for (param_name, source) in &webhook.extract {
        if let Some(value) = extract_param(source, &query) {
            params.insert(param_name.clone(), serde_json::Value::String(value));
        }
    }

    let action = &webhook.action;
    let webhook_agent_id = format!("webhook:{}", webhook.name);
    let security_ctx = webhook_security_context(&webhook.name, authenticated);

    // Gate 2 — authorization (ADR-0156). Authorize the action through the same
    // Cedar path as the OData write binding, using the current entity view as
    // the resource. A tenant with policies loaded is default-deny, so an
    // unpermitted webhook principal is rejected here.
    let resource_attrs = match state
        .load_authz_resource_snapshot(&tenant, &entity_type, &entity_id)
        .await
    {
        Ok(snapshot) => snapshot.resource_attrs,
        Err(_) => minimal_resource_attrs(&entity_id),
    };

    if let Err(denial) = state.authorize_with_context(
        &security_ctx,
        action,
        &entity_type,
        &resource_attrs,
        tenant.as_str(),
    ) {
        let reason = denial.to_string();
        tracing::warn!(webhook = %webhook.name, action = %action, "webhook authorization denied: {reason}");
        // Only surface a governance pending-decision for a caller that proved
        // it holds the signing secret. An unauthenticated caller (a webhook
        // with no declared secret) must not be able to amplify pending-decision
        // records by spamming the public route; it gets a plain 403.
        if !authenticated {
            return (StatusCode::FORBIDDEN, reason);
        }
        let from_status = resource_attrs
            .get("status")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let decision = record_authz_denial(
            &state,
            DenialInput {
                tenant: tenant.as_str(),
                security_ctx: &security_ctx,
                agent_id_override: Some(webhook_agent_id.as_str()),
                action,
                resource_type: &entity_type,
                resource_id: &entity_id,
                resource_attrs: serde_json::to_value(&resource_attrs).unwrap_or_default(),
                reason: &reason,
                module_name: None,
                from_status,
            },
        )
        .await;
        return (
            StatusCode::FORBIDDEN,
            format!("{reason} (decision: {})", decision.id),
        );
    }

    let agent_ctx = AgentContext {
        security_ctx: Some(security_ctx),
        agent_id: Some(webhook_agent_id),
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

/// Verify the webhook's HMAC-SHA256 signature over the raw request body.
///
/// Returns:
/// - `Ok(true)` — a declared secret verified the request signature.
/// - `Ok(false)` — the webhook declares no `hmac_secret`; authenticity is left
///   to the Cedar gate (default-deny in production unless explicitly permitted).
/// - `Err((status, message))` — a secret is declared but the request is
///   unsigned, mis-signed, or the secret cannot be resolved. Fail closed: an
///   unverifiable signed webhook must not dispatch.
fn verify_webhook_signature(
    state: &ServerState,
    tenant: &TenantId,
    webhook: &Webhook,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<bool, (StatusCode, String)> {
    let Some(secret_template) = webhook.hmac_secret.as_deref() else {
        return Ok(false);
    };

    let secret = resolve_webhook_secret(state, tenant, secret_template).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            "Webhook signing secret is not configured".to_string(),
        )
    })?;

    let header_name = webhook
        .hmac_header
        .as_deref()
        .unwrap_or(DEFAULT_SIGNATURE_HEADER);
    let provided = headers
        .get(header_name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                format!("Missing webhook signature header '{header_name}'"),
            )
        })?;

    if signature_matches(&secret, body, provided) {
        Ok(true)
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            "Webhook signature verification failed".to_string(),
        ))
    }
}

/// Resolve the webhook signing secret from the tenant secret store.
///
/// Supports `{secret:KEY}` templates and literal secrets. Returns `None` (fail
/// closed) when no vault is configured, the template is unresolved, or the
/// resolved value is empty.
fn resolve_webhook_secret(
    state: &ServerState,
    tenant: &TenantId,
    template: &str,
) -> Option<String> {
    let vault = state.secrets_vault.as_ref()?;
    let mut one = BTreeMap::new();
    one.insert("secret".to_string(), template.to_string());
    let resolved = resolve_secret_templates(&one, vault, tenant.as_str());
    let value = resolved.get("secret")?.clone();
    if value.is_empty() || value.contains("{secret:") {
        return None;
    }
    Some(value)
}

/// Constant-time comparison of a provided webhook signature against the
/// expected `HMAC-SHA256(secret, body)`.
///
/// Accepts an optional `sha256=` prefix and is case-insensitive over the hex
/// digest. Uses [`subtle::ConstantTimeEq`] rather than `==` to avoid a timing
/// side channel on the digest.
fn signature_matches(secret: &str, body: &[u8], provided: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use subtle::ConstantTimeEq;

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let expected_hex = hex::encode(mac.finalize().into_bytes());
    debug_assert_eq!(
        expected_hex.len(),
        64,
        "HMAC-SHA256 hex digest must be 64 characters"
    );

    // Normalize first (lowercase) so the `sha256=` prefix is matched
    // case-insensitively, then strip it.
    let normalized = provided.trim().to_ascii_lowercase();
    let provided_hex = normalized
        .strip_prefix("sha256=")
        .unwrap_or(normalized.as_str())
        .trim();

    provided_hex
        .as_bytes()
        .ct_eq(expected_hex.as_bytes())
        .into()
}

/// Build the restricted Cedar principal for a webhook caller.
///
/// The principal is an `Agent` named `webhook:{name}` with role/agent_type
/// `webhook`; the `authenticated` attribute records whether an HMAC signature
/// was verified. It is never `System`, so the same tenant Cedar policies that
/// gate the OData write path also gate this route.
fn webhook_security_context(webhook_name: &str, authenticated: bool) -> SecurityContext {
    let mut ctx = SecurityContext::from_headers(&[]);
    ctx.principal.id = format!("webhook:{webhook_name}");
    ctx.principal.kind = PrincipalKind::Agent;
    ctx.principal.role = Some("webhook".to_string());
    ctx.principal.agent_type = Some("webhook".to_string());
    ctx.principal.attributes.insert(
        "authenticated".to_string(),
        serde_json::Value::Bool(authenticated),
    );
    ctx.context_attrs.insert(
        "authenticated".to_string(),
        serde_json::Value::Bool(authenticated),
    );
    ctx.with_action_context(format!("webhook:{webhook_name}"))
}

/// Minimal Cedar resource view for a webhook target that does not yet exist.
fn minimal_resource_attrs(entity_id: &str) -> BTreeMap<String, serde_json::Value> {
    let mut attrs = BTreeMap::new();
    attrs.insert(
        "id".to_string(),
        serde_json::Value::String(entity_id.to_string()),
    );
    attrs.insert(
        "status".to_string(),
        serde_json::Value::String(String::new()),
    );
    attrs
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
mod receiver_test;
