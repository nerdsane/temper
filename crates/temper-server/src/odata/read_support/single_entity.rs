//! Single-entity reads against the entity catalog.
//!
//! The catalog answers a point read only for an entity whose actor is not in
//! memory; a loaded actor is ahead of the catalog by up to one queued
//! projection write (ARN-522). `catalog_body_ignoring_actor` is the one
//! exception, for the path that already tried the actor and lost it to
//! passivation.

use super::{
    catalog_row_to_entity_body, maybe_spawn_catalog_shadow_check,
    should_read_catalog_for_materialization, try_load_catalog_rows,
};
use crate::state::ServerState;
use temper_runtime::tenant::TenantId;

/// Try to load a single entity body from the durable `entity_catalog`.
///
/// Returns `Some(json)` when the catalog has a row for `(tenant, entity_type,
/// key)` and catalog materialization is preferred or the catalog fast-read
/// feature flag is enabled. Returns `None` when catalog reads are disabled,
/// the catalog has no row, or the read fails — caller is expected to fall
/// back to actor hydration in that case.
///
/// The returned JSON has the same shape as the actor's serialized
/// `EntityState` so downstream code (`enrich_entity_response`, OData
/// clients, blob hydration) can't tell the difference.
pub(in crate::odata) async fn try_load_entity_body_from_catalog(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_set_name: &str,
    key: &str,
    prefer_catalog: bool,
) -> Option<serde_json::Value> {
    if !should_read_catalog_for_materialization(prefer_catalog) {
        return None;
    }
    // A loaded actor is ahead of the catalog by up to one queued projection
    // write; a read that follows a dispatch must see that dispatch (ARN-522).
    if state.has_loaded_actor(tenant, entity_type, key) {
        return None;
    }
    let ids = [key.to_string()];
    let rows = try_load_catalog_rows(state, tenant, entity_type, &ids).await;
    let row = rows.into_iter().next().map(|(_, r)| r)?;
    maybe_spawn_catalog_shadow_check(state, tenant, entity_type, &row);
    Some(catalog_row_to_entity_body(
        entity_type,
        entity_set_name,
        row,
    ))
}

/// The catalog's answer for one key regardless of whether the actor is
/// loaded. Only for the path that already tried the actor and lost it to
/// passivation (ARN-522, round 3); ordinary reads go through
/// `try_load_entity_body_from_catalog`.
pub(in crate::odata) async fn catalog_body_ignoring_actor(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_set_name: &str,
    key: &str,
) -> Option<serde_json::Value> {
    let ids = [key.to_string()];
    let row = try_load_catalog_rows(state, tenant, entity_type, &ids)
        .await
        .into_iter()
        .next()
        .map(|(_, r)| r)?;
    Some(catalog_row_to_entity_body(
        entity_type,
        entity_set_name,
        row,
    ))
}
