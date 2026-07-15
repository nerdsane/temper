//! Idempotent query-projection repairs discovered during authoritative reads.

use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

pub(super) async fn remove_deleted_projection(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) {
    let Some(query_plane) = state.query_plane_store() else {
        return;
    };
    if let Err(error) = query_plane
        .remove_projection(tenant.as_str(), entity_type, entity_id)
        .await
    {
        tracing::debug!(
            error = %error,
            tenant = %tenant,
            entity_type,
            entity_id,
            "failed to repair deleted query projection during OData materialization"
        );
    }
}
