//! Shared helpers for OData read handlers.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use futures_util::stream::{self, StreamExt};
use temper_runtime::tenant::TenantId;

use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::state::ServerState;
use crate::storage::EntityCatalogRow;

mod shadow;

use shadow::{
    CatalogShadowReadBudget, maybe_spawn_catalog_shadow_check,
    maybe_spawn_catalog_shadow_check_with_budget,
};

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name) // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name) // determinism-ok: read once at startup
        .ok()
        .and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

pub(super) fn odata_default_page_size() -> usize {
    static DEFAULT_PAGE_SIZE: OnceLock<usize> = OnceLock::new();
    *DEFAULT_PAGE_SIZE.get_or_init(|| env_usize("TEMPER_ODATA_DEFAULT_PAGE_SIZE", 100))
}

pub(super) fn odata_max_entities() -> usize {
    static MAX_ENTITIES: OnceLock<usize> = OnceLock::new();
    *MAX_ENTITIES.get_or_init(|| env_usize("TEMPER_ODATA_MAX_ENTITIES", 1000))
}

fn entity_set_materialization_concurrency() -> usize {
    static CONCURRENCY: OnceLock<usize> = OnceLock::new();
    *CONCURRENCY
        .get_or_init(|| env_usize("TEMPER_ODATA_ENTITY_SET_MATERIALIZATION_CONCURRENCY", 16))
}

/// When true, list materialization reads each row from the durable
/// `entity_catalog` projection in a single batch query, avoiding the per-row
/// actor hydration cost. IDs that are absent from the catalog still fall
/// back to the actor path on a per-id basis.
fn catalog_fast_read_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_bool("TEMPER_ODATA_CATALOG_FAST_READ", false))
}

fn should_read_catalog_for_materialization(prefer_catalog: bool) -> bool {
    prefer_catalog || catalog_fast_read_enabled()
}

/// Build the OData JSON body for a single catalog row.
///
/// Mirrors the actor's serialized `EntityState` shape (`status`, `fields`,
/// `entity_type`, `entity_id`, `sequence_nr`, `total_event_count`,
/// `item_count`, `counters`, `booleans`, `lists`, `events`) so downstream
/// `enrich_entity_response` and OData clients see the same payload as the
/// actor path. Counters/booleans/lists are emitted as empty maps because the
/// catalog only persists `status` + `fields`; entity types that depend on
/// those collections in their OData response should leave the fast-read
/// flag off until the projection is extended.
fn catalog_row_to_entity_body(
    entity_type: &str,
    entity_set_name: &str,
    row: EntityCatalogRow,
) -> serde_json::Value {
    let id = row.entity_id;
    serde_json::json!({
        "entity_type": entity_type,
        "entity_id": id,
        "status": row.status,
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": row.fields,
        "events": [],
        "total_event_count": row.sequence_nr,
        "sequence_nr": row.sequence_nr,
        "@odata.id": format!("{entity_set_name}('{id}')"),
    })
}

async fn try_load_catalog_rows(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_ids: &[String],
) -> BTreeMap<String, EntityCatalogRow> {
    let Some(query_plane) = state.query_plane_store() else {
        return BTreeMap::new();
    };
    match query_plane
        .load_entity_catalog_rows(tenant.as_str(), entity_type, entity_ids)
        .await
    {
        Ok(Some(rows)) => rows
            .into_iter()
            .map(|row| (row.entity_id.clone(), row))
            .collect(),
        Ok(None) => BTreeMap::new(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                tenant = %tenant,
                entity_type = %entity_type,
                "catalog fast-read failed; falling back to actor materialization"
            );
            BTreeMap::new()
        }
    }
}

/// Try to load a single entity body from the durable `entity_catalog`.
///
/// Returns `Some(json)` when the catalog has a row for `(tenant, entity_type,
/// key)` and the catalog fast-read feature flag is enabled. Returns `None`
/// when the flag is off, the catalog has no row, or the read fails — caller
/// is expected to fall back to actor hydration in that case.
///
/// The returned JSON has the same shape as the actor's serialized
/// `EntityState` so downstream code (`enrich_entity_response`, OData
/// clients, blob hydration) can't tell the difference.
pub(super) async fn try_load_entity_body_from_catalog(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_set_name: &str,
    key: &str,
) -> Option<serde_json::Value> {
    if !catalog_fast_read_enabled() {
        return None;
    }
    let ids = [key.to_string()];
    let rows = try_load_catalog_rows(state, tenant, entity_type, &ids).await;
    let row = rows.into_iter().next().map(|(_, r)| r)?;
    maybe_spawn_catalog_shadow_check(state, tenant, entity_type, &row);
    let mut body = catalog_row_to_entity_body(entity_type, entity_set_name, row);
    hydrate_blob_refs_for_tenant(state, tenant, &mut body).await;
    Some(body)
}

pub(super) async fn materialize_entity_set_entities(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_set_name: &str,
    entity_ids: &[String],
    prefer_catalog: bool,
) -> MaterializedEntitySet {
    let mut catalog_hits: BTreeMap<String, EntityCatalogRow> =
        if should_read_catalog_for_materialization(prefer_catalog) {
            try_load_catalog_rows(state, tenant, entity_type, entity_ids).await
        } else {
            BTreeMap::new()
        };
    let mut shadow_budget = CatalogShadowReadBudget::for_entity_set();

    let concurrency = entity_set_materialization_concurrency();
    let entities = stream::iter(entity_ids.iter().cloned())
        .map(|id| {
            let catalog_row = catalog_hits.remove(&id);
            if let Some(row) = catalog_row.as_ref() {
                let _ = maybe_spawn_catalog_shadow_check_with_budget(
                    state,
                    tenant,
                    entity_type,
                    row,
                    &mut shadow_budget,
                );
            }
            let state = state.clone();
            let tenant = tenant.clone();
            let entity_type = entity_type.to_string();
            let entity_set_name = entity_set_name.to_string();
            async move {
                if let Some(row) = catalog_row {
                    let mut entity =
                        catalog_row_to_entity_body(&entity_type, &entity_set_name, row);
                    hydrate_blob_refs_for_tenant(&state, &tenant, &mut entity).await;
                    return Some(entity);
                }
                match state
                    .get_tenant_entity_state(&tenant, &entity_type, &id)
                    .await
                {
                    Ok(response) => {
                        let mut entity = serde_json::to_value(&response.state).unwrap_or_default();
                        hydrate_blob_refs_for_tenant(&state, &tenant, &mut entity).await;
                        if let Some(obj) = entity.as_object_mut() {
                            obj.insert(
                                "@odata.id".into(),
                                serde_json::json!(format!("{entity_set_name}('{id}')")),
                            );
                        }
                        Some(entity)
                    }
                    Err(error) => {
                        tracing::debug!(
                            error = %error,
                            tenant = %tenant,
                            entity_type = %entity_type,
                            entity_id = %id,
                            "failed to materialize entity for OData collection"
                        );
                        None
                    }
                }
            }
        })
        .buffered(concurrency)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    MaterializedEntitySet {
        entities,
        catalog_shadow_check_budget: shadow_budget.configured(),
        catalog_shadow_check_scheduled: shadow_budget.scheduled(),
    }
}

pub(super) struct MaterializedEntitySet {
    pub(super) entities: Vec<serde_json::Value>,
    pub(super) catalog_shadow_check_budget: usize,
    pub(super) catalog_shadow_check_scheduled: usize,
}

pub(super) fn select_entity_ids_for_materialization(
    mut entity_ids: Vec<String>,
    query_options: &temper_odata::query::types::QueryOptions,
    default_page_size: usize,
    max_entities: usize,
) -> (
    Vec<String>,
    temper_odata::query::types::QueryOptions,
    Option<usize>,
) {
    let has_filter_or_order = query_options.filter.is_some() || query_options.orderby.is_some();
    let mut precomputed_count = None;

    let apply_options = if !has_filter_or_order {
        let total_available = entity_ids.len();
        if query_options.count == Some(true) {
            precomputed_count = Some(total_available);
        }

        let skip = query_options.skip.unwrap_or(0);
        let top = query_options.top.unwrap_or(default_page_size);
        let requested = top.min(max_entities);
        entity_ids = entity_ids
            .into_iter()
            .skip(skip)
            .take(requested)
            .collect::<Vec<_>>();

        let mut adjusted = query_options.clone();
        adjusted.skip = None;
        adjusted.top = None;
        adjusted.count = None;
        adjusted
    } else {
        // When a $filter or $orderby is present, we must materialise ALL
        // candidate entities before the filter/sort can be applied.
        // Truncating the candidate set before filtering would silently hide
        // entities that match the filter but sort past the cutoff — a
        // correctness bug that caused system skills to vanish from large File
        // collections (see ADR: skill-bootstrap-invisible-in-odata).
        //
        // Safety cap: we still impose a hard ceiling (10× max_entities) to
        // prevent unbounded materialisation in pathological cases, but log a
        // warning so the operator knows results may be incomplete.
        let safety_cap = max_entities.saturating_mul(10);
        if entity_ids.len() > safety_cap {
            tracing::warn!(
                total = entity_ids.len(),
                safety_cap,
                "OData filtered query exceeds safety cap — results may be incomplete; \
                 raise TEMPER_ODATA_MAX_ENTITIES to cover all entities"
            );
            entity_ids.truncate(safety_cap);
        }

        let mut adjusted = query_options.clone();
        if adjusted.top.is_none() {
            adjusted.top = Some(default_page_size);
        } else if let Some(top) = adjusted.top {
            adjusted.top = Some(top.min(max_entities));
        }
        adjusted
    };

    (entity_ids, apply_options, precomputed_count)
}

/// Resolve an entity set name from an entity type name.
///
/// Reverse-lookups the entity_set_map to find the set name for a given type.
pub(super) fn resolve_entity_set_name(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> String {
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    if let Some(tc) = registry.get_tenant(tenant) {
        for (set_name, type_name) in &tc.entity_set_map {
            if type_name == entity_type {
                return set_name.clone();
            }
        }
    }
    // Fallback: pluralize entity type
    format!("{entity_type}s")
}

/// Record a trajectory entry for an EntitySetNotFound error.
pub(super) async fn record_entity_set_not_found(state: &ServerState, tenant: &str, set_name: &str) {
    tracing::warn!(tenant = %tenant, entity_set = %set_name, "entity set not found");
    // Intentionally no trajectory write: read-only operations must not write to the database.
    // Previously this wrote a TrajectoryEntry on every failed EntitySetLookup, creating
    // unbounded junk rows (4,269 rows, 83% of all trajectories) from phantom entity polling.
    let _ = state; // suppress unused warning
}

#[cfg(test)]
mod tests;
