//! Count-only entity totals for checking an app's records across a binary
//! handoff.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::AuthenticatedRequestContext;

use super::require_admin_operation;
use crate::state::ServerState;

/// GET /_admin/entity-counts
///
/// Durable totals per entity type for the caller's own tenant, read from the
/// query-plane catalog with one grouped count. The body carries only type
/// names and numbers: no entity ids, fields or values. Granted to System,
/// Admin and verified operators by the built-in system-platform policy, so no
/// app's Cedar changes.
pub(super) async fn read_entity_counts(
    State(state): State<ServerState>,
    Extension(authenticated): Extension<AuthenticatedRequestContext>,
) -> Response {
    let tenant = authenticated.tenant().to_string();
    if let Err(status) = require_admin_operation(
        &state,
        &authenticated,
        "read_entity_counts",
        "EntityCounts",
        &tenant,
        BTreeMap::new(),
    ) {
        return status.into_response();
    }
    let Some(query_plane) = state.query_plane_store() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match query_plane.projected_entity_counts_by_type(&tenant).await {
        Ok(Some(counts)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "tenant": tenant, "counts": counts })),
        )
            .into_response(),
        Ok(None) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(error) => {
            tracing::error!(tenant = %tenant, error = %error, "entity counts read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use temper_authz::{AuthenticatedRequestContext, SecurityContext};
    use temper_runtime::ActorSystem;
    use temper_runtime::tenant::TenantId;
    use temper_spec::csdl::parse_csdl;
    use tower::ServiceExt;

    use crate::state::ServerState;

    fn test_server_state() -> ServerState {
        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let csdl = parse_csdl(csdl_xml).unwrap();
        let system = ActorSystem::new("entity-counts-test");
        ServerState::new(system, csdl, csdl_xml.to_string())
    }

    fn context_request(
        mut request: Request<Body>,
        tenant: &str,
        security_context: SecurityContext,
    ) -> Request<Body> {
        request
            .extensions_mut()
            .insert(AuthenticatedRequestContext::new(
                TenantId::new(tenant),
                security_context,
            ));
        request
    }

    fn customer_context() -> SecurityContext {
        SecurityContext {
            principal: temper_authz::Principal {
                id: "rita".to_string(),
                kind: temper_authz::PrincipalKind::Customer,
                role: None,
                acting_for: None,
                agent_type: None,
                attributes: Default::default(),
            },
            context_attrs: Default::default(),
            correlation_id: "entity-counts-customer".to_string(),
        }
    }

    /// A server whose `acme` tenant carries an app-shaped Cedar bundle that
    /// names nothing about entity counts, over a Turso catalog holding two
    /// live Orders, one deleted Order, one Invoice and another tenant's Order.
    async fn entity_counts_state(dir: &std::path::Path) -> ServerState {
        let store = temper_store_turso::TursoEventStore::new(
            &format!("file:{}", dir.join("counts.db").display()),
            None,
        )
        .await
        .expect("store opens");
        let fields = serde_json::json!({ "Title": "private title" });
        for id in ["order-1", "order-2", "order-3"] {
            store
                .upsert_query_projection("acme", "Order", id, "Open", &fields, 1)
                .await
                .expect("upsert order");
        }
        store
            .remove_query_projection("acme", "Order", "order-3")
            .await
            .expect("delete order");
        store
            .upsert_query_projection("acme", "Invoice", "invoice-1", "Draft", &fields, 1)
            .await
            .expect("upsert invoice");
        store
            .upsert_query_projection("other", "Order", "other-1", "Open", &fields, 1)
            .await
            .expect("upsert other tenant");

        let mut state = test_server_state();
        state.set_storage_stack(crate::StorageStack::from_turso(store));
        state
            .authz
            .reload_tenant_policies("acme", "permit(principal, action, resource is Order);")
            .expect("app policy should parse");
        state
    }

    fn entity_counts_request() -> Request<Body> {
        Request::builder()
            .uri("/_admin/entity-counts")
            .body(Body::empty())
            .expect("request should build")
    }

    #[tokio::test]
    async fn entity_counts_answer_totals_and_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = crate::router::build_router(entity_counts_state(dir.path()).await);

        let response = app
            .oneshot(context_request(
                entity_counts_request(),
                "acme",
                SecurityContext::from_resolved_identity("operator", "operator", None),
            ))
            .await
            .expect("request should run");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json,
            serde_json::json!({
                "tenant": "acme",
                "counts": { "Invoice": 1, "Order": 2 }
            })
        );
        let text = String::from_utf8_lossy(&body);
        for leaked in ["order-1", "invoice-1", "private title", "Open", "Draft"] {
            assert!(!text.contains(leaked), "body must not carry {leaked}");
        }
    }

    #[tokio::test]
    async fn entity_counts_refuse_customers_and_unverified_operator_claims() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = crate::router::build_router(entity_counts_state(dir.path()).await);

        let unverified_operator = SecurityContext::anonymous().with_agent_context(
            Some("operator"),
            None,
            Some("operator"),
        );
        for denied in [
            customer_context(),
            unverified_operator,
            SecurityContext::from_resolved_identity("worker-1", "worker", None),
        ] {
            let response = app
                .clone()
                .oneshot(context_request(entity_counts_request(), "acme", denied))
                .await
                .expect("request should run");
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
}
