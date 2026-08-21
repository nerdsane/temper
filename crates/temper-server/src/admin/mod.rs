//! Admin endpoints for runtime tuning of platform primitives.
//!
//! Currently exposes a single endpoint for admission-cap overrides per
//! ADR-0051 sub-decision 5. SRE can retune caps without a redeploy when a
//! customer saturates unexpectedly.

use std::collections::BTreeMap;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::{get, patch};
use axum::{Json, Router};

use temper_authz::AuthenticatedRequestContext;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Admission;

use crate::authz::require_tenant_match;
use crate::profiling::{CpuProfileQuery, cpu_profile_handler, wall_profile_handler};
use crate::state::ServerState;

/// Build the `/admin` sub-router.
pub fn build_admin_router() -> Router<ServerState> {
    Router::new()
        .route(
            "/admission/{tenant}/{entity_type}",
            patch(override_admission),
        )
        // ADR-0055: on-demand CPU and wall-clock profile capture.
        // Gated by TEMPER_PROFILING_ENABLED at request time.
        .route("/profile/cpu", get(capture_cpu_profile))
        .route("/profile/wall", get(capture_wall_profile))
}

fn require_admin_operation(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    mut resource_attrs: BTreeMap<String, serde_json::Value>,
) -> Result<(), StatusCode> {
    let security_context = authenticated.security_context();
    resource_attrs.insert(
        "id".to_string(),
        serde_json::Value::String(resource_id.to_string()),
    );
    resource_attrs.insert(
        "tenant".to_string(),
        serde_json::Value::String(authenticated.tenant().to_string()),
    );
    state
        .authorize_with_context(
            security_context,
            action,
            resource_type,
            &resource_attrs,
            authenticated.tenant().as_str(),
        )
        .map_err(|denial| {
            tracing::warn!(
                reason = %denial,
                tenant = %authenticated.tenant(),
                principal_id = %security_context.principal.id,
                action,
                resource_type,
                resource_id,
                "admin operation denied"
            );
            StatusCode::FORBIDDEN
        })?;
    Ok(())
}

/// PATCH /admin/admission/{tenant}/{entity_type}
///
/// Body is a JSON `Admission` struct. Empty body (`null` or `{}`) un-sets
/// the runtime override and falls back to the spec-declared caps.
async fn override_admission(
    State(state): State<ServerState>,
    Extension(authenticated): Extension<AuthenticatedRequestContext>,
    Path((tenant, entity_type)): Path<(String, String)>,
    Json(admission): Json<Option<Admission>>,
) -> impl IntoResponse {
    if let Err(status) = require_tenant_match(&authenticated, &tenant) {
        return status.into_response();
    }
    let tenant_id = TenantId::from(tenant.clone());
    let resource_id = format!("{tenant}/{entity_type}");
    if let Err(status) = require_admin_operation(
        &state,
        &authenticated,
        "manage_admission",
        "AdmissionControl",
        &resource_id,
        BTreeMap::from([
            (
                "targetTenant".to_string(),
                serde_json::Value::String(tenant_id.to_string()),
            ),
            (
                "entityType".to_string(),
                serde_json::Value::String(entity_type.clone()),
            ),
        ]),
    ) {
        return status.into_response();
    }
    state
        .admission
        .override_caps(&entity_type, admission.clone())
        .await;
    tracing::info!(
        tenant = %tenant,
        entity_type = %entity_type,
        caps = ?admission,
        "admission override applied (ADR-0051)"
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "tenant": tenant,
            "entity_type": entity_type,
            "admission": admission,
        })),
    )
        .into_response()
}

async fn capture_cpu_profile(
    State(state): State<ServerState>,
    Extension(authenticated): Extension<AuthenticatedRequestContext>,
    query: Query<CpuProfileQuery>,
) -> Response {
    if let Err(status) = require_admin_operation(
        &state,
        &authenticated,
        "capture_profile",
        "Profiler",
        "cpu",
        BTreeMap::new(),
    ) {
        return status.into_response();
    }
    cpu_profile_handler(query).await
}

async fn capture_wall_profile(
    State(state): State<ServerState>,
    Extension(authenticated): Extension<AuthenticatedRequestContext>,
    query: Query<CpuProfileQuery>,
) -> Response {
    if let Err(status) = require_admin_operation(
        &state,
        &authenticated,
        "capture_profile",
        "Profiler",
        "wall",
        BTreeMap::new(),
    ) {
        return status.into_response();
    }
    wall_profile_handler(query).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use temper_authz::{AuthenticatedRequestContext, SecurityContext};
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;
    use tower::ServiceExt;

    fn test_server_state() -> ServerState {
        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let csdl = parse_csdl(csdl_xml).unwrap();
        let system = ActorSystem::new("admin-test");
        ServerState::new(system, csdl, csdl_xml.to_string())
    }

    fn authenticated_request(mut request: Request<Body>) -> Request<Body> {
        request
            .extensions_mut()
            .insert(AuthenticatedRequestContext::new(
                TenantId::new("acme"),
                SecurityContext::system(),
            ));
        request
    }

    fn operator_request(mut request: Request<Body>, tenant: &str) -> Request<Body> {
        request
            .extensions_mut()
            .insert(AuthenticatedRequestContext::new(
                TenantId::new(tenant),
                SecurityContext::from_resolved_identity("operator", "operator", None),
            ));
        request
    }

    fn claimed_admin_request(mut request: Request<Body>, tenant: &str) -> Request<Body> {
        request
            .extensions_mut()
            .insert(AuthenticatedRequestContext::new(
                TenantId::new(tenant),
                SecurityContext {
                    principal: temper_authz::Principal {
                        id: "claimed-admin".to_string(),
                        kind: temper_authz::PrincipalKind::Admin,
                        role: None,
                        acting_for: None,
                        agent_type: None,
                        attributes: Default::default(),
                    },
                    context_attrs: Default::default(),
                    correlation_id: "admin-side-channel-test".to_string(),
                },
            ));
        request
    }

    #[tokio::test]
    async fn typed_admin_kind_does_not_bypass_cedar() {
        let app = crate::router::build_router(test_server_state());
        let response = app
            .oneshot(claimed_admin_request(
                Request::builder()
                    .method("PATCH")
                    .uri("/_admin/admission/acme/Session")
                    .header("Content-Type", "application/json")
                    .body(Body::from("null"))
                    .expect("request should build"),
                "acme",
            ))
            .await
            .expect("request should run");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admin_override_applies_and_clears_caps() {
        let state = test_server_state();
        let app = crate::router::build_router(state.clone());

        // Apply: override admission for Session with Submit=2 cap.
        let body = serde_json::json!({
            "max_concurrent_creates": 5,
            "max_concurrent_actions": { "Submit": 2 },
            "queue_depth": 10,
            "queue_timeout_seconds": 1
        });
        let resp = app
            .clone()
            .oneshot(authenticated_request(
                Request::builder()
                    .method("PATCH")
                    .uri("/_admin/admission/acme/Session")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Clear: null body unsets.
        let resp = app
            .oneshot(authenticated_request(
                Request::builder()
                    .method("PATCH")
                    .uri("/_admin/admission/acme/Session")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::Value::Null).unwrap(),
                    ))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn operator_uses_resource_specific_cedar_authority() {
        let state = test_server_state();
        state
            .authz
            .reload_tenant_policies(
                "acme",
                r#"
permit(
  principal == Agent::"operator",
  action == Action::"manage_admission",
  resource == AdmissionControl::"acme/Session"
);
permit(
  principal == Agent::"operator",
  action == Action::"capture_profile",
  resource == Profiler::"cpu"
);
"#,
            )
            .expect("operator policy should parse");
        let app = crate::router::build_router(state);

        let allowed = app
            .clone()
            .oneshot(operator_request(
                Request::builder()
                    .method("PATCH")
                    .uri("/_admin/admission/acme/Session")
                    .header("Content-Type", "application/json")
                    .body(Body::from("null"))
                    .expect("request should build"),
                "acme",
            ))
            .await
            .expect("request should run");
        assert_eq!(allowed.status(), StatusCode::OK);

        let wrong_resource = app
            .clone()
            .oneshot(operator_request(
                Request::builder()
                    .method("PATCH")
                    .uri("/_admin/admission/acme/Other")
                    .header("Content-Type", "application/json")
                    .body(Body::from("null"))
                    .expect("request should build"),
                "acme",
            ))
            .await
            .expect("request should run");
        assert_eq!(wrong_resource.status(), StatusCode::FORBIDDEN);

        let wrong_tenant = app
            .clone()
            .oneshot(operator_request(
                Request::builder()
                    .method("PATCH")
                    .uri("/_admin/admission/acme/Session")
                    .header("Content-Type", "application/json")
                    .body(Body::from("null"))
                    .expect("request should build"),
                "other",
            ))
            .await
            .expect("request should run");
        assert_eq!(wrong_tenant.status(), StatusCode::UNAUTHORIZED);

        let allowed_profile_but_disabled = app
            .clone()
            .oneshot(operator_request(
                Request::builder()
                    .uri("/_admin/profile/cpu?seconds=1")
                    .body(Body::empty())
                    .expect("request should build"),
                "acme",
            ))
            .await
            .expect("request should run");
        assert_eq!(
            allowed_profile_but_disabled.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        let unauthorized_profile_resource = app
            .oneshot(operator_request(
                Request::builder()
                    .uri("/_admin/profile/wall?seconds=1")
                    .body(Body::empty())
                    .expect("request should build"),
                "acme",
            ))
            .await
            .expect("request should run");
        assert_eq!(
            unauthorized_profile_resource.status(),
            StatusCode::FORBIDDEN
        );
    }
}
