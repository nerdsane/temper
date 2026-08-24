//! REST API for tenant management.
//!
//! Routes:
//! - `POST   /api/tenants`              — create/provision a new tenant
//! - `GET    /api/tenants`              — list all tenants
//! - `DELETE /api/tenants/:id`          — remove a tenant
//! - `POST   /api/tenants/:id/users`    — add a user to a tenant
//! - `DELETE /api/tenants/:id/users/:user_id` — remove a user from a tenant
//! - `GET    /api/tenants/:id/users`    — list users for a tenant
//! - `GET    /api/genesis/apps/follow-updates` — list staged follow-latest rollout status

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use temper_authz::AuthenticatedRequestContext;
use temper_server::storage::TursoStoreProvider;

use crate::state::PlatformState;

mod apps;
mod auth;
pub(crate) use apps::{
    garbage_collect_local_bundle_cache, get_genesis_app_bundle, get_os_app_guide,
    install_genesis_app, install_local_bundle, list_genesis_follow_updates, list_os_apps,
};
use auth::{
    PlatformResourceAuthorization, require_authenticated, require_control_plane,
    require_resource_authorization, require_same_tenant, validate_tenant_id,
};

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

fn turso_provider(state: &PlatformState) -> Option<Arc<dyn TursoStoreProvider>> {
    state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.clone())
}

fn authorization_error(status: StatusCode) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({
            "error": if status == StatusCode::UNAUTHORIZED {
                "authentication required"
            } else {
                "authorization denied"
            }
        })),
    )
}

/// Build the tenant management API router.
pub fn tenant_api_router() -> Router<PlatformState> {
    Router::new()
        .route("/tenants", routing::post(create_tenant).get(list_tenants))
        .route("/tenants/{id}", routing::delete(delete_tenant))
        .route(
            "/tenants/{id}/users",
            routing::post(add_user).get(list_users),
        )
        .route(
            "/tenants/{id}/users/{user_id}",
            routing::delete(remove_user),
        )
        .route("/os-apps", routing::get(list_os_apps))
        .route("/os-apps/{name}", routing::get(get_os_app_guide))
        .route(
            "/app-bundles/install",
            routing::post(install_local_bundle).layer(DefaultBodyLimit::max(
                crate::app_bundles::MAX_BUNDLE_REQUEST_BYTES,
            )),
        )
        .route(
            "/app-bundles/cache/gc",
            routing::post(garbage_collect_local_bundle_cache),
        )
        .route("/genesis/apps/install", routing::post(install_genesis_app))
        .route(
            "/genesis/apps/follow-updates",
            routing::get(list_genesis_follow_updates),
        )
        .route(
            "/genesis/apps/{owner}/{name}/versions/{hash}/bundle",
            routing::get(get_genesis_app_bundle),
        )
}

/// `POST /api/tenants` — provision a new tenant database.
async fn create_tenant(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(req): Json<CreateTenantRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = validate_tenant_id(&req.tenant_id)
        .and_then(|_| require_control_plane(authenticated))
        .and_then(|_| {
            require_resource_authorization(
                &state,
                authenticated,
                PlatformResourceAuthorization {
                    action: "create_tenant",
                    resource_type: "Tenant",
                    resource_id: &req.tenant_id,
                    attrs: std::collections::BTreeMap::from([(
                        "targetTenant".to_string(),
                        serde_json::Value::String(req.tenant_id.clone()),
                    )]),
                },
            )
        })
    {
        return authorization_error(status);
    }
    let Some(provider) = turso_provider(&state) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    if !provider.supports_tenant_admin() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "tenant management requires routed storage mode"})),
        );
    }

    match provider.register_tenant(&req.tenant_id).await {
        Ok(_store) => {
            // Bootstrap agent specs for the new tenant.
            // New tenant — no prior verification cache.
            crate::bootstrap_agent_specs(
                &state,
                &req.tenant_id,
                false,
                &std::collections::BTreeMap::new(),
            );
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
async fn list_tenants(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_control_plane(authenticated).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "list_tenants",
                resource_type: "TenantCatalog",
                resource_id: "all",
                attrs: std::collections::BTreeMap::new(),
            },
        )
    }) {
        return authorization_error(status);
    }
    let Some(provider) = turso_provider(&state) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    if !provider.supports_tenant_admin() {
        return (
            StatusCode::OK,
            Json(serde_json::json!(TenantListResponse { tenants: vec![] })),
        );
    }

    match provider.list_tenants().await {
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

/// `DELETE /api/tenants/:id` — remove a tenant and its data.
pub(crate) async fn delete_tenant(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_same_tenant(authenticated, &tenant_id).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "delete_tenant",
                resource_type: "Tenant",
                resource_id: &tenant_id,
                attrs: std::collections::BTreeMap::new(),
            },
        )
    }) {
        return authorization_error(status);
    }
    let Some(provider) = turso_provider(&state) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    if !provider.supports_tenant_admin() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "tenant management requires routed storage mode"})),
        );
    }

    // Remove from persistence (Turso registry + users).
    match provider.remove_tenant(&tenant_id).await {
        Ok(true) => {
            // Also remove from in-memory SpecRegistry.
            let tid = temper_runtime::tenant::TenantId::new(&tenant_id);
            {
                let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
                registry.remove_tenant(&tid);
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "deleted": true,
                    "tenant_id": tenant_id,
                })),
            )
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("tenant '{tenant_id}' not found")})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `POST /api/tenants/:id/users` — add a user to a tenant.
async fn add_user(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
    Json(req): Json<AddUserRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    let user_resource_id = format!("{tenant_id}/{}", req.user_id);
    if let Err(status) = require_same_tenant(authenticated, &tenant_id).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "manage_tenant_users",
                resource_type: "TenantUser",
                resource_id: &user_resource_id,
                attrs: std::collections::BTreeMap::from([
                    (
                        "targetTenant".to_string(),
                        serde_json::Value::String(tenant_id.clone()),
                    ),
                    (
                        "userId".to_string(),
                        serde_json::Value::String(req.user_id.clone()),
                    ),
                    (
                        "role".to_string(),
                        serde_json::Value::String(req.role.clone()),
                    ),
                ]),
            },
        )
    }) {
        return authorization_error(status);
    }
    let Some(provider) = turso_provider(&state) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    if !provider.supports_tenant_admin() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "tenant management requires routed storage mode"})),
        );
    }

    match provider
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_same_tenant(authenticated, &tenant_id).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "read_tenant_users",
                resource_type: "Tenant",
                resource_id: &tenant_id,
                attrs: std::collections::BTreeMap::new(),
            },
        )
    }) {
        return authorization_error(status);
    }
    let Some(provider) = turso_provider(&state) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    if !provider.supports_tenant_admin() {
        return (StatusCode::OK, Json(serde_json::json!({"users": []})));
    }

    match provider.list_tenant_users(&tenant_id).await {
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::extract::Path((tenant_id, user_id)): axum::extract::Path<(String, String)>,
) -> StatusCode {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status,
    };
    let resource_id = format!("{tenant_id}/{user_id}");
    if let Err(status) = require_same_tenant(authenticated, &tenant_id).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "manage_tenant_users",
                resource_type: "TenantUser",
                resource_id: &resource_id,
                attrs: std::collections::BTreeMap::from([
                    (
                        "targetTenant".to_string(),
                        serde_json::Value::String(tenant_id.clone()),
                    ),
                    (
                        "userId".to_string(),
                        serde_json::Value::String(user_id.clone()),
                    ),
                ]),
            },
        )
    }) {
        return status;
    }
    let Some(provider) = turso_provider(&state) else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };

    if !provider.supports_tenant_admin() {
        return StatusCode::BAD_REQUEST;
    }

    match provider.remove_tenant_user(&tenant_id, &user_id).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests;
