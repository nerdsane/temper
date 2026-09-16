//! Repair a missing query projection from the actor that just answered a read.
//!
//! Only a catalog *miss* is repaired here. A row the read skipped because the
//! entity's actor is loaded belongs to the projection queue, which carries the
//! newest sequence; writing it back from a read could put an older row over a
//! newer queued write (ARN-522).

use crate::state::ServerState;
use temper_runtime::tenant::TenantId;

pub(super) async fn repair_from_actor(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    entity_state: &crate::entity_actor::types::EntityState,
) {
    if entity_state.status == "Deleted" {
        return;
    }
    let Some(query_plane) = state.query_plane_store() else {
        return;
    };
    let fields = state.query_projection_fields(tenant, entity_type, &entity_state.fields);
    let projected_state = state.query_projection_state(entity_state);
    if let Err(error) = query_plane
        .upsert_projection(
            tenant.as_str(),
            entity_type,
            entity_id,
            &entity_state.status,
            &fields,
            &projected_state,
            entity_state.sequence_nr,
        )
        .await
    {
        tracing::debug!(
            error = %error,
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            "failed to repair query projection after actor materialization fallback"
        );
    }
}
