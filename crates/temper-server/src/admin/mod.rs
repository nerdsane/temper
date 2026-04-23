//! Admin endpoints for runtime tuning of platform primitives.
//!
//! Currently exposes a single endpoint for admission-cap overrides per
//! ADR-0051 sub-decision 5. SRE can retune caps without a redeploy when a
//! customer saturates unexpectedly.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::patch;
use axum::{Json, Router};

use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Admission;

use crate::state::ServerState;

/// Build the `/admin` sub-router.
pub fn build_admin_router() -> Router<ServerState> {
    Router::new().route(
        "/admission/{tenant}/{entity_type}",
        patch(override_admission),
    )
}

/// PATCH /admin/admission/{tenant}/{entity_type}
///
/// Body is a JSON `Admission` struct. Empty body (`null` or `{}`) un-sets
/// the runtime override and falls back to the spec-declared caps.
async fn override_admission(
    State(state): State<ServerState>,
    Path((tenant, entity_type)): Path<(String, String)>,
    Json(admission): Json<Option<Admission>>,
) -> impl IntoResponse {
    let _tenant_id = TenantId::from(tenant.clone());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;
    use tower::ServiceExt;

    fn test_server_state() -> ServerState {
        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let csdl = parse_csdl(csdl_xml).unwrap();
        let system = ActorSystem::new("admin-test");
        ServerState::new(system, csdl, csdl_xml.to_string())
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
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/_admin/admission/acme/Session")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Clear: null body unsets.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/_admin/admission/acme/Session")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::Value::Null).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
