//! Prospective generic-write authorization uses the actor's exact field ownership rules.
use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;

use crate::{
    ServerState,
    entity_actor::{EntityActor, EntityState, effects},
};

fn table(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> Result<Arc<TransitionTable>, String> {
    state
        .registry
        .read()
        .map_err(|error| error.to_string())?
        .get_table(tenant, entity_type)
        .or_else(|| state.transition_tables.get(entity_type).cloned())
        .ok_or_else(|| format!("No transition table for {tenant}/{entity_type}"))
}

/// Normalize non-strict creation while retaining strict inputs for validation.
pub(super) fn create_fields(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    fields: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let table = table(state, tenant, entity_type)?;
    if table.strict_action_params {
        // Do not sanitize away fields that strict creation must reject.
        return Ok(fields);
    }
    Ok(EntityActor::build_initial_state(entity_type, entity_id, &table, &fields).fields)
}

/// Build the exact field view a generic actor write would commit.
pub(super) fn prospective_fields(
    state: &ServerState,
    tenant: &TenantId,
    mut current: EntityState,
    fields: &serde_json::Value,
    replace: bool,
) -> Result<serde_json::Value, String> {
    let table = table(state, tenant, &current.entity_type)?;
    if !effects::apply_field_update(&mut current, &table, fields, replace) {
        return Err("entity field update must be a JSON object".into());
    }
    Ok(current.fields)
}
