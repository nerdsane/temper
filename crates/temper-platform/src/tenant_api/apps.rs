use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;

use super::auth::{
    PlatformResourceAuthorization, require_authenticated, require_resource_authorization,
    require_same_tenant,
};
use super::authorization_error;
use crate::state::PlatformState;

#[derive(serde::Deserialize)]
pub(crate) struct BundleCacheGcRequest {
    tenant: String,
    #[serde(default)]
    dry_run: bool,
}

/// Collect local bundle objects not reachable from durable provenance.
pub(crate) async fn garbage_collect_local_bundle_cache(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(req): Json<BundleCacheGcRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_same_tenant(authenticated, &req.tenant).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "manage_app_cache",
                resource_type: "AppCache",
                resource_id: "local",
                attrs: std::collections::BTreeMap::new(),
            },
        )
    }) {
        return authorization_error(status);
    }
    match crate::app_bundles::garbage_collect_local_bundle_cache(&state, req.dry_run).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        ),
    }
}

/// Install one immutable local bundle into the credential-bound tenant.
pub(crate) async fn install_local_bundle(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(req): Json<crate::app_bundles::InstallBundleRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_same_tenant(authenticated, &req.tenant).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "install_app_bundle",
                resource_type: "App",
                resource_id: &req.manifest.bundle_digest,
                attrs: std::collections::BTreeMap::from([
                    (
                        "targetTenant".to_string(),
                        serde_json::Value::String(req.tenant.clone()),
                    ),
                    (
                        "sourceKind".to_string(),
                        serde_json::Value::String("local_bundle".to_string()),
                    ),
                ]),
            },
        )
    }) {
        return authorization_error(status);
    }
    match crate::app_bundles::install_local_bundle(&state, req).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        ),
    }
}

/// List the credential tenant's authorized application catalog.
pub(crate) async fn list_os_apps(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_catalog",
            resource_type: "AppCatalog",
            resource_id: "all",
            attrs: std::collections::BTreeMap::new(),
        },
    ) {
        return authorization_error(status);
    }
    let apps = crate::os_apps::list_os_apps();
    (StatusCode::OK, Json(serde_json::json!({ "apps": apps })))
}

/// Return one authorized application guide.
pub(crate) async fn get_os_app_guide(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_catalog",
            resource_type: "AppCatalogEntry",
            resource_id: &name,
            attrs: std::collections::BTreeMap::new(),
        },
    ) {
        return authorization_error(status);
    }
    match crate::os_apps::get_app_guide(&name) {
        Some(guide) => (
            StatusCode::OK,
            Json(serde_json::json!({"name": name, "guide": guide})),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("No app guide found for '{name}'"),
            })),
        ),
    }
}

/// Return follow-latest state only for the credential-bound tenant.
pub(crate) async fn list_genesis_follow_updates(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    let tenant = authenticated.tenant().as_str();
    if let Err(status) = require_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_installs",
            resource_type: "AppInstall",
            resource_id: tenant,
            attrs: std::collections::BTreeMap::new(),
        },
    ) {
        return authorization_error(status);
    }
    let updates = crate::genesis_install::list_genesis_follow_latest_updates(&state)
        .await
        .into_iter()
        .filter(|update| update.tenant == tenant)
        .collect::<Vec<_>>();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "value": updates })),
    )
}

/// Install one pinned application into the credential-bound tenant.
pub(crate) async fn install_genesis_app(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(req): Json<crate::genesis_install::GenesisRegistryInstallRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_same_tenant(authenticated, &req.tenant).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "install_app",
                resource_type: "App",
                resource_id: &req.app_ref,
                attrs: std::collections::BTreeMap::from([
                    (
                        "targetTenant".to_string(),
                        serde_json::Value::String(req.tenant.clone()),
                    ),
                    (
                        "registryTenant".to_string(),
                        serde_json::Value::String(req.registry_tenant.clone()),
                    ),
                ]),
            },
        )
    }) {
        return authorization_error(status);
    }
    match crate::genesis_install::install_genesis_app_from_registry(&state, req).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))),
        Err(error) if error.contains("not found") => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": error })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error })),
        ),
    }
}

/// Export one pinned application bundle from the credential-bound registry tenant.
pub(crate) async fn get_genesis_app_bundle(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path((owner, name, hash)): Path<(String, String, String)>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    let registry_tenant = authenticated.tenant().as_str();
    let resource_id = format!("{owner}/{name}@{hash}");
    if let Err(status) = require_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_bundle",
            resource_type: "App",
            resource_id: &resource_id,
            attrs: std::collections::BTreeMap::from([
                (
                    "owner".to_string(),
                    serde_json::Value::String(owner.clone()),
                ),
                ("name".to_string(), serde_json::Value::String(name.clone())),
                (
                    "versionHash".to_string(),
                    serde_json::Value::String(hash.clone()),
                ),
            ]),
        },
    ) {
        return authorization_error(status);
    }
    match crate::genesis_install::export_genesis_registry_bundle(
        &state,
        registry_tenant,
        &owner,
        &name,
        &hash,
    )
    .await
    {
        Ok(bundle) => (StatusCode::OK, Json(serde_json::json!(bundle))),
        Err(error) if error.contains("not found") => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": error })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error })),
        ),
    }
}
