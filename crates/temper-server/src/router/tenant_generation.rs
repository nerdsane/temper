//! Tenant-generation admission and publication barriers.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use temper_runtime::tenant::TenantId;

use crate::odata;
use crate::state::ServerState;

/// Hold a tenant generation stable for the full OData handler. Publications
/// take the matching write side of this barrier, so an in-flight request sees
/// either the complete old generation or the complete new one—never a registry,
/// Cedar, reaction, and WASM mixture.
pub(super) async fn stable_tenant_generation(
    State(state): State<ServerState>,
    mut request: Request,
    next: Next,
) -> Response {
    let tenant = match odata::extract_tenant(request.headers(), &state) {
        Ok(tenant) => tenant,
        Err(error) => return error.into_response(),
    };
    if state.spec_publication_gated(&tenant) {
        if !gated_publication_retry_allowed(&state, &tenant, &request) {
            return publication_in_progress_response();
        }
        let Some(generation) = state.try_begin_tenant_request(&tenant).await else {
            return publication_in_progress_response();
        };
        if !state.spec_publication_gated(&tenant) {
            return publication_in_progress_response();
        }
        let captured_generation = state.tenant_generation_version(&tenant);
        let lease =
            crate::state::TenantGenerationLease::new(&tenant, generation, captured_generation);
        request.extensions_mut().insert(lease.clone());
        let response = next.run(request).await;
        lease.release();
        return response;
    }
    let Some(generation) = state.try_begin_tenant_request(&tenant).await else {
        return publication_in_progress_response();
    };
    if state.spec_publication_gated(&tenant) {
        return publication_in_progress_response();
    }
    let captured_generation = state.tenant_generation_version(&tenant);
    let lease = crate::state::TenantGenerationLease::new(&tenant, generation, captured_generation);
    request.extensions_mut().insert(lease.clone());
    let response = next.run(request).await;
    lease.release();
    response
}

fn gated_publication_retry_allowed(
    state: &ServerState,
    tenant: &TenantId,
    request: &Request,
) -> bool {
    if request.method() != Method::POST {
        return false;
    }
    let Some(_idempotency_key) = request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let path = request
        .uri()
        .path()
        .strip_prefix("/tdata/")
        .unwrap_or(request.uri().path());
    let Ok(temper_odata::path::ODataPath::BoundAction { parent, action }) =
        temper_odata::path::parse_path(path)
    else {
        return false;
    };
    let temper_odata::path::ODataPath::Entity(set_name, key) = *parent else {
        return false;
    };
    match key {
        temper_odata::path::KeyValue::Single(_) => {}
        temper_odata::path::KeyValue::Composite(_) => return false,
    }
    let Some(entity_type) = odata::resolve_entity_type(state, tenant, &set_name) else {
        return false;
    };
    if !state
        .bound_action_hook
        .as_ref()
        .is_some_and(|hook| hook.requires_generation_handoff(&entity_type, &action))
    {
        return false;
    }
    true
}

pub(super) fn publication_in_progress_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({
            "error": {
                "code": "SpecPublicationInProgress",
                "message": "Tenant runtime generation is being published; retry the request",
            }
        })),
    )
        .into_response()
}
