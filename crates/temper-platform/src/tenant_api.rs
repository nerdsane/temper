//! REST API for tenant management.
//!
//! Routes:
//! - `POST /api/tenants`              — create/provision a new tenant
//! - `GET  /api/tenants`              — list all tenants
//! - `POST /api/tenants/:id/users`    — add a user to a tenant
//! - `DELETE /api/tenants/:id/users/:user_id` — remove a user from a tenant
//! - `GET  /api/tenants/:id/users`    — list users for a tenant

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};

use serde::{Deserialize, Serialize};

use crate::state::PlatformState;

/// Request body for `POST /api/tenants`.
#[derive(Debug, Deserialize)]
pub struct CreateTenantRequest {
    pub tenant_id: String,
}

/// Response body for tenant creation.
#[derive(Debug, Serialize)]
pub struct CreateTenantResponse {
    pub tenant_id: String,
    pub status: String,
}

/// Response body for tenant listing.
#[derive(Debug, Serialize)]
pub struct TenantListResponse {
    pub tenants: Vec<TenantInfo>,
}

/// Summary of a registered tenant.
#[derive(Debug, Serialize)]
pub struct TenantInfo {
    pub tenant_id: String,
    pub status: String,
}

/// Request body for `POST /api/tenants/:id/users`.
#[derive(Debug, Deserialize)]
pub struct AddUserRequest {
    pub user_id: String,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "member".to_string()
}

/// Response body for user operations.
#[derive(Debug, Serialize)]
pub struct UserInfo {
    pub tenant_id: String,
    pub user_id: String,
    pub role: String,
}

/// Build the tenant management API router.
pub fn tenant_api_router() -> Router<PlatformState> {
    Router::new()
        .route("/tenants", routing::post(create_tenant).get(list_tenants))
        .route(
            "/tenants/{id}/users",
            routing::post(add_user).get(list_users),
        )
        .route(
            "/tenants/{id}/users/{user_id}",
            routing::delete(remove_user),
        )
}

/// `POST /api/tenants` — provision a new tenant database.
async fn create_tenant(
    State(state): State<PlatformState>,
    Json(req): Json<CreateTenantRequest>,
) -> impl IntoResponse {
    let Some(ref store) = state.server.event_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    let Some(router) = store.tenant_router() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "tenant management requires routed storage mode"})),
        );
    };

    match router.register_tenant(&req.tenant_id).await {
        Ok(_store) => {
            // Bootstrap agent specs for the new tenant.
            crate::bootstrap_agent_specs(&state, &req.tenant_id);
            (
                StatusCode::CREATED,
                Json(serde_json::json!(CreateTenantResponse {
                    tenant_id: req.tenant_id,
                    status: "active".to_string(),
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `GET /api/tenants` — list all registered tenants.
async fn list_tenants(State(state): State<PlatformState>) -> impl IntoResponse {
    let Some(ref store) = state.server.event_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    let Some(router) = store.tenant_router() else {
        return (
            StatusCode::OK,
            Json(serde_json::json!(TenantListResponse { tenants: vec![] })),
        );
    };

    match router.list_tenants().await {
        Ok(ids) => {
            let tenants = ids
                .into_iter()
                .map(|id| TenantInfo {
                    tenant_id: id,
                    status: "active".to_string(),
                })
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::json!(TenantListResponse { tenants })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `POST /api/tenants/:id/users` — add a user to a tenant.
async fn add_user(
    State(state): State<PlatformState>,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
    Json(req): Json<AddUserRequest>,
) -> impl IntoResponse {
    let Some(ref store) = state.server.event_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    let Some(router) = store.tenant_router() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "tenant management requires routed storage mode"})),
        );
    };

    match router
        .add_tenant_user(&tenant_id, &req.user_id, &req.role)
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!(UserInfo {
                tenant_id,
                user_id: req.user_id,
                role: req.role,
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `GET /api/tenants/:id/users` — list users for a tenant.
async fn list_users(
    State(state): State<PlatformState>,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let Some(ref store) = state.server.event_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    let Some(router) = store.tenant_router() else {
        return (StatusCode::OK, Json(serde_json::json!({"users": []})));
    };

    match router.list_tenant_users(&tenant_id).await {
        Ok(rows) => {
            let users: Vec<UserInfo> = rows
                .into_iter()
                .map(|r| UserInfo {
                    tenant_id: r.tenant_id,
                    user_id: r.user_id,
                    role: r.role,
                })
                .collect();
            (StatusCode::OK, Json(serde_json::json!({"users": users})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `DELETE /api/tenants/:id/users/:user_id` — remove a user from a tenant.
async fn remove_user(
    State(state): State<PlatformState>,
    axum::extract::Path((tenant_id, user_id)): axum::extract::Path<(String, String)>,
) -> impl IntoResponse {
    let Some(ref store) = state.server.event_store else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };

    let Some(router) = store.tenant_router() else {
        return StatusCode::BAD_REQUEST;
    };

    match router.remove_tenant_user(&tenant_id, &user_id).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
