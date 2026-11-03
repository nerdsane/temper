//! One read decision for every observe route that returns a single entity.

use axum::http::StatusCode;
use temper_authz::AuthenticatedRequestContext;

use crate::entity_actor::EntityResponse;
use crate::state::ServerState;

/// Load an existing entity and authorize reading it, or answer `404`.
pub(super) async fn require_existing_entity_read(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    entity_type: &str,
    entity_id: &str,
) -> Result<(), StatusCode> {
    let body = crate::odata::load_existing_entity_descriptor_body(
        state,
        authenticated.tenant(),
        entity_type,
        entity_type,
        entity_id,
    )
    .await
    .map_err(|_| StatusCode::NOT_FOUND)?;
    authorize_entity_read(state, authenticated, entity_type, entity_id, &body)
}

/// Read an existing entity's current state and authorize returning it.
///
/// Answers `404` for an id that does not exist or was deleted, and never
/// starts an entity for one (see [`ServerState::get_existing_entity_state`]).
/// Routes that answer more than once (a wait that wakes, a stream that sends
/// later events) call this for each answer, so every decision is made on a
/// state no older than what is sent.
pub(super) async fn load_authorized_entity_state(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    entity_type: &str,
    entity_id: &str,
) -> Result<EntityResponse, StatusCode> {
    let entity = state
        .get_existing_entity_state(authenticated.tenant(), entity_type, entity_id)
        .await
        .ok()
        .flatten()
        .ok_or(StatusCode::NOT_FOUND)?;
    authorize_entity_state(state, authenticated, entity_type, entity_id, &entity)?;
    Ok(entity)
}

/// Authorize returning `entity`, the state just read from its actor: `404`
/// when that state is the deletion tombstone, else the read decision on it.
pub(super) fn authorize_entity_state(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    entity_type: &str,
    entity_id: &str,
    entity: &EntityResponse,
) -> Result<(), StatusCode> {
    if entity.state.status == "Deleted" {
        return Err(StatusCode::NOT_FOUND);
    }
    let body =
        serde_json::to_value(&entity.state).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    authorize_entity_read(state, authenticated, entity_type, entity_id, &body)
}

/// Make the same Cedar read decision as `GET /tdata/{Set}('{id}')`.
///
/// The resource is the exact entity id with the entity's fields and status as
/// attributes, so a policy that names one entity or one of its fields decides
/// observe reads exactly as it decides OData reads.
pub(super) fn authorize_entity_read(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    entity_type: &str,
    entity_id: &str,
    body: &serde_json::Value,
) -> Result<(), StatusCode> {
    crate::odata::authz::authorize_read(
        state,
        authenticated.tenant(),
        authenticated.security_context(),
        crate::odata::authz::READ_ACTION,
        entity_type,
        entity_id,
        body,
    )
    .map_err(|_| {
        tracing::warn!(entity_type, entity_id, "observe entity read denied");
        StatusCode::FORBIDDEN
    })
}
