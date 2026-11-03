//! Constraint reads follow the target entity's configured runtime. Pending
//! spawn results take precedence because the current write creates them.

use serde_json::Value;
use temper_actor_runtime::spec_actor::SpecActorState;
use temper_runtime::tenant::TenantId;

use super::PendingRecords;
use crate::state::ServerState;

/// The committed values constraint checks read from either actor runtime.
pub(super) struct RelatedRecord {
    /// The runtime state machine status.
    pub status: String,
    /// The fields stored by the target actor.
    pub fields: Value,
}

/// Read a target without creating an absent record or falling back to another
/// runtime. Missing runtimes, unreadable stores and invalid state fail closed.
pub(super) async fn read_related_record(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    pending: &PendingRecords,
) -> Result<Option<RelatedRecord>, String> {
    if let Some(record) = pending.get(entity_type, entity_id) {
        return Ok(Some(RelatedRecord {
            status: record.status.clone(),
            fields: record.fields.clone(),
        }));
    }
    if state.is_pg_actor_backed(tenant, entity_type) {
        let system = state
            .pg_actor_system
            .as_ref()
            .ok_or_else(|| "related record's Postgres actor runtime is unavailable".to_string())?;
        let Some(bytes) = system
            .load_state(&format!("{tenant}/{entity_id}"), entity_type)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let record: SpecActorState = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid related actor state: {error}"))?;
        return Ok(Some(RelatedRecord {
            status: record.status,
            fields: record.fields,
        }));
    }
    if !state
        .ensure_entity_loaded(tenant, entity_type, entity_id)
        .await
    {
        return Ok(None);
    }
    let record = state
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await?
        .state;
    Ok(Some(RelatedRecord {
        status: record.status,
        fields: record.fields,
    }))
}
