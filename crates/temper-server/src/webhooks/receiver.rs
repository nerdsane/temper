//! Inbound webhook receiver: another system's request becomes one entity
//! action (ADR-0182).
//!
//! Any method on `/webhooks/{tenant}/{*path}` reaches the webhook declared at
//! that path. The request's signature is checked with the provider's own
//! scheme ([`super::signature`]) before anything in it is read. The entity id
//! and the action's parameters then come from the query string, the headers
//! or the JSON body, as the declaration names them, and the action runs as
//! `Agent::"webhook:{name}"` only when the tenant's Cedar permits it.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use serde_json::{Map, Value};
use tracing::instrument;

use super::signature::{SignedRequest, verify_signature};
use crate::request_context::AgentContext;
use crate::secrets::template::resolve_secret_templates;
use crate::state::ServerState;
use crate::state::redecide::Redecide;
use temper_authz::SecurityContext;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::{Webhook, WebhookSource};

/// Handle an inbound webhook request.
///
/// Route: any method on `/webhooks/{tenant}/{*path}`. The body is bounded
/// only by the router's request-body limit (axum's `DefaultBodyLimit`),
/// which answers 413 itself when a body exceeds it.
///
/// Answers 404 when no webhook is declared at the path, 405 when the
/// webhook declares another method, 401 when the signature does not check
/// out, 400 when the entity id is missing or a body the webhook reads is not
/// JSON, 403 when Cedar does not permit the action, and otherwise the
/// action's response.
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
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let tenant = TenantId::new(&tenant_str);

    let Some((entity_type, webhook)) = find_webhook(&state, &tenant, &webhook_path) else {
        tracing::warn!(path = %webhook_path, "no webhook registered at path");
        return (
            StatusCode::NOT_FOUND,
            format!("No webhook registered at path '{webhook_path}' for tenant '{tenant_str}'"),
        );
    };

    if let Some(expected) = webhook.method.as_deref()
        && !method.as_str().eq_ignore_ascii_case(expected)
    {
        tracing::warn!(expected = %expected, actual = %method, "webhook method mismatch");
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            format!(
                "Webhook '{}' expects {} but received {}",
                webhook.name,
                expected.to_ascii_uppercase(),
                method.as_str()
            ),
        );
    }

    let path_and_query = uri
        .path_and_query()
        .map_or_else(|| uri.path(), |pq| pq.as_str());
    let signed = SignedRequest {
        method: method.as_str(),
        path_and_query,
        headers: &headers,
        body: &body,
    };
    let Some(security_ctx) = admit_webhook(&state, &tenant, &webhook, &signed) else {
        return (
            StatusCode::UNAUTHORIZED,
            "Webhook admission denied".to_string(),
        );
    };

    // Only now, with the signature checked, is anything in the request read.
    let (entity_id, params) = match read_action_input(&webhook, &query, &headers, &body) {
        Ok(input) => input,
        Err(reason) => {
            tracing::warn!(webhook = %webhook.name, reason = %reason, "webhook request unreadable");
            return (StatusCode::BAD_REQUEST, reason);
        }
    };

    let action = &webhook.action;
    let mut resource_attrs = BTreeMap::new();
    resource_attrs.insert("id".to_string(), Value::String(entity_id.clone()));
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

    // The policy above reads only the record's id; the record's own checks run
    // in the dispatch, on the record as it is. A dispatch decided on a record
    // that changed first is decided again on it (`state::redecide`).
    let params = Value::Object(params);
    let mut redecide = Redecide::start(&state);
    let dispatched = loop {
        let dispatched = state
            .dispatch_outside_action(crate::state::DispatchCommand {
                tenant: &tenant,
                entity_type: &entity_type,
                entity_id: &entity_id,
                action,
                params: params.clone(),
                agent_ctx: &agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await;
        let stale = matches!(dispatched, Err(crate::state::DispatchError::StaleWrite(_)));
        if !redecide.again(stale, "webhook") {
            break dispatched;
        }
    };
    match dispatched {
        Ok(response) => {
            let body = serde_json::to_string(&response).unwrap_or_default();
            (StatusCode::OK, body)
        }
        Err(crate::state::DispatchError::StaleWrite(stale)) => {
            tracing::warn!(error = %stale, webhook = %webhook.name, "webhook action refused as stale");
            (StatusCode::CONFLICT, stale.body().to_string())
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

/// Class B admission: a signature from the webhook's secret is required and
/// fail-closed. Every refusal is logged with its reason; the sender sees 401.
fn admit_webhook(
    state: &ServerState,
    tenant: &TenantId,
    webhook: &Webhook,
    request: &SignedRequest<'_>,
) -> Option<SecurityContext> {
    // Specs load only with a secret (ADR-0182); this guards specs stored
    // before that check existed.
    let Some(secret_template) = webhook.hmac_secret.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(webhook = %webhook.name, "webhook missing hmac_secret");
        return None;
    };
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!(webhook = %webhook.name, "webhook secret required but no secrets vault");
        return None;
    };
    let mut templates = BTreeMap::new();
    templates.insert("hmac".to_string(), secret_template.to_string());
    let resolved = resolve_secret_templates(&templates, vault, tenant.as_str());
    let secret = resolved.get("hmac").cloned().unwrap_or_default();
    if secret.is_empty() || secret.contains("{secret:") {
        tracing::warn!(webhook = %webhook.name, "webhook secret did not resolve");
        return None;
    }
    let now_unix_seconds = sim_now().timestamp();
    if let Err(rejection) = verify_signature(
        webhook.signature,
        &secret,
        webhook.hmac_header.as_deref(),
        request,
        now_unix_seconds,
    ) {
        tracing::warn!(
            webhook = %webhook.name,
            scheme = webhook.signature.as_str(),
            reason = %rejection,
            "webhook signature refused"
        );
        return None;
    }
    Some(SecurityContext::from_resolved_identity(
        &format!("webhook:{}", webhook.name),
        "webhook",
        None,
    ))
}

/// The entity id and action parameters the webhook declares, read from a
/// request whose signature has been checked. A parameter whose source is
/// absent (or a JSON `null`) is left out; the action's own contract decides
/// whether it may be.
///
/// # Errors
///
/// The body is read and is not JSON, or the entity id is absent, empty or
/// neither text nor a number.
fn read_action_input(
    webhook: &Webhook,
    query: &BTreeMap<String, String>,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(String, Map<String, Value>), String> {
    let reads_body = std::iter::once(&webhook.entity_id)
        .chain(webhook.extract.values())
        .any(|source| matches!(WebhookSource::parse(source), Some(WebhookSource::Body(_))));
    let json = if reads_body {
        let parsed = serde_json::from_slice::<Value>(body).map_err(|e| {
            format!(
                "Webhook '{}' reads the body, which is not JSON: {e}",
                webhook.name
            )
        })?;
        Some(parsed)
    } else {
        None
    };
    let read = |source: &str| read_source(source, query, headers, json.as_ref());

    let entity_id = match read(&webhook.entity_id) {
        Some(Value::String(id)) if !id.is_empty() => id,
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(_)) | None => {
            return Err(format!(
                "Missing entity id: the request has nothing at '{}'",
                webhook.entity_id
            ));
        }
        Some(other) => {
            return Err(format!(
                "Entity id at '{}' is {other}, not text or a number",
                webhook.entity_id
            ));
        }
    };
    let mut params = Map::new();
    for (name, source) in &webhook.extract {
        if let Some(value) = read(source) {
            params.insert(name.clone(), value);
        }
    }
    debug_assert!(!entity_id.is_empty());
    debug_assert!(params.len() <= webhook.extract.len());
    Ok((entity_id, params))
}

/// The value a source names: query and header values as text, body values
/// as their JSON. `None` when absent or a JSON `null`.
fn read_source(
    source: &str,
    query: &BTreeMap<String, String>,
    headers: &HeaderMap,
    body: Option<&Value>,
) -> Option<Value> {
    let value = match WebhookSource::parse(source)? {
        WebhookSource::Query(name) => Value::String(query.get(name)?.clone()),
        WebhookSource::Header(name) => Value::String(headers.get(name)?.to_str().ok()?.to_string()),
        WebhookSource::Body(path) => json_at(body?, path)?.clone(),
    };
    (!value.is_null()).then_some(value)
}

/// The value at a dot-separated path into `value` (the whole value for an
/// empty path); a number segment indexes an array.
fn json_at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    if path.is_empty() {
        return Some(value);
    }
    path.split('.')
        .try_fold(value, |current, segment| match current {
            Value::Object(fields) => fields.get(segment),
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?),
            _ => None,
        })
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

#[cfg(test)]
#[path = "receiver_test.rs"]
mod tests;
