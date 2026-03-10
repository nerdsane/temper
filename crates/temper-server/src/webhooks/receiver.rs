//! Inbound webhook receiver for external callbacks.
//!
//! Handles GET/POST requests to `/webhooks/{tenant}/{*path}` and dispatches
//! entity actions based on webhook declarations in IOA specs. This enables
//! OAuth2 callbacks, payment gateway notifications, and other external system
//! integrations to trigger entity state transitions.

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;

use tracing::instrument;

use crate::request_context::AgentContext;
use crate::state::ServerState;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Webhook;

/// Handle an inbound webhook request.
///
/// Route: `GET|POST /webhooks/{tenant}/{*path}`
///
/// The handler looks up the webhook configuration from the tenant's spec
/// registry, validates the HTTP method, extracts the entity ID and action
/// parameters, then dispatches the configured action to the target entity.
#[instrument(skip_all, fields(tenant, webhook_path, otel.name = "GET|POST /webhooks/{tenant}/{*path}"))]
pub async fn handle_webhook(
    method: Method,
    State(state): State<ServerState>,
    Path((tenant_str, webhook_path)): Path<(String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
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

    // Extract action parameters from the configured extraction map.
    let mut params = serde_json::Map::new();
    for (param_name, source) in &webhook.extract {
        if let Some(value) = extract_param(source, &query) {
            params.insert(param_name.clone(), serde_json::Value::String(value));
        }
    }

    let action = &webhook.action;
    let agent_ctx = AgentContext {
        agent_id: Some(format!("webhook:{}", webhook.name)),
        session_id: None,
        agent_type: None,
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
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;
    use tower::ServiceExt;

    const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");

    /// IOA spec with a webhook declaration for OAuth callback.
    const ORDER_IOA_WITH_WEBHOOK: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed", "Cancelled", "Authorized"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"

[[action]]
name = "ConfirmOrder"
kind = "input"
from = ["Submitted"]
to = "Confirmed"

[[action]]
name = "CancelOrder"
kind = "input"
from = ["Draft", "Submitted"]
to = "Cancelled"

[[action]]
name = "HandleOAuthCallback"
kind = "input"
from = ["Submitted"]
to = "Authorized"
params = ["code"]

[[webhook]]
name = "oauth_callback"
path = "oauth/callback"
method = "GET"
action = "HandleOAuthCallback"
entity_lookup = "query_param"
entity_param = "state"

[webhook.extract]
code = "query.code"
"#;

    fn build_test_state() -> ServerState {
        let csdl = parse_csdl(CSDL_XML).unwrap();
        let system = ActorSystem::new("webhook-test");
        let state = ServerState::new(system, csdl, CSDL_XML.to_string());

        // Register tenant with webhook-enabled spec.
        {
            let mut registry = state.registry.write().unwrap();
            let csdl2 = parse_csdl(CSDL_XML).unwrap();
            registry.register_tenant(
                "test-tenant",
                csdl2,
                CSDL_XML.to_string(),
                &[("Order", ORDER_IOA_WITH_WEBHOOK)],
            );
        }

        state
    }

    fn build_test_router() -> axum::Router {
        crate::router::build_router(build_test_state())
    }

    #[tokio::test]
    async fn webhook_dispatches_action() {
        let state = build_test_state();
        let tenant = TenantId::new("test-tenant");

        // Create entity directly via dispatch.
        let _create = state
            .get_or_create_tenant_entity(
                &tenant,
                "Order",
                "ent-1",
                serde_json::json!({"id": "ent-1"}),
            )
            .await
            .expect("entity creation should succeed");

        // Submit to move to "Submitted".
        let submit = state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                "ent-1",
                "SubmitOrder",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await
            .expect("SubmitOrder should succeed");
        assert!(submit.success, "SubmitOrder should succeed");
        assert_eq!(submit.state.status, "Submitted");

        // Build router and call webhook.
        let app = crate::router::build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/webhooks/test-tenant/oauth/callback?state=ent-1&code=abc123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["success"].as_bool().unwrap_or(false),
            "HandleOAuthCallback should succeed"
        );
        assert_eq!(json["state"]["status"], "Authorized");
    }

    #[tokio::test]
    async fn webhook_missing_entity_id_returns_400() {
        let app = build_test_router();

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/webhooks/test-tenant/oauth/callback?code=abc123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn webhook_unknown_path_returns_404() {
        let app = build_test_router();

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/webhooks/test-tenant/nonexistent/path?entity_id=ent-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn webhook_extracts_query_params() {
        let query: BTreeMap<String, String> = [
            ("code".to_string(), "auth-code-123".to_string()),
            ("state".to_string(), "entity-id".to_string()),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            extract_param("query.code", &query),
            Some("auth-code-123".to_string())
        );
        assert_eq!(
            extract_param("query.state", &query),
            Some("entity-id".to_string())
        );
        assert_eq!(extract_param("query.missing", &query), None);
    }
}
