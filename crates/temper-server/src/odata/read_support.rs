//! Shared helpers for OData read handlers.

use std::collections::{BTreeMap, BTreeSet};

use futures_util::stream::{self, StreamExt};
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;
use crate::storage::{
    CatalogRowsLoad, EntityCatalogRow, load_catalog_rows_by_id, load_selected_catalog_rows_by_id,
};

mod config;
mod select_projection;
mod shadow;

use config::{catalog_fast_read_enabled, entity_set_materialization_concurrency};
pub(super) use config::{odata_default_page_size, odata_max_entities};
use select_projection::catalog_row_to_selected_entity_body;
#[cfg(test)]
pub(super) use select_projection::catalog_select_projection_fields;
use shadow::{
    CatalogShadowReadBudget, maybe_spawn_catalog_shadow_check,
    maybe_spawn_catalog_shadow_check_with_budget,
};

fn should_read_catalog_for_materialization(prefer_catalog: bool) -> bool {
    prefer_catalog || catalog_fast_read_enabled()
}

/// Build the OData JSON body for a single catalog row.
///
/// Prefer the full projected `EntityState` payload when present. Older rows
/// can still be synthesized from `status` + `fields` during rolling deploys,
/// but those legacy rows do not carry counters, booleans, lists, item counts,
/// or fields omitted from the query projection.
fn catalog_row_to_entity_body(
    entity_type: &str,
    entity_set_name: &str,
    row: EntityCatalogRow,
) -> serde_json::Value {
    let id = row.entity_id.clone();
    if let Some(mut state) = row.state
        && let Some(obj) = state.as_object_mut()
    {
        obj.insert("entity_type".to_string(), serde_json::json!(entity_type));
        obj.insert("entity_id".to_string(), serde_json::json!(id.clone()));
        obj.insert("status".to_string(), serde_json::json!(row.status));
        obj.entry("fields".to_string()).or_insert(row.fields);
        obj.entry("item_count".to_string())
            .or_insert(serde_json::json!(0));
        obj.entry("counters".to_string())
            .or_insert(serde_json::json!({}));
        obj.entry("booleans".to_string())
            .or_insert(serde_json::json!({}));
        obj.entry("lists".to_string())
            .or_insert(serde_json::json!({}));
        obj.insert("events".to_string(), serde_json::json!([]));
        obj.entry("total_event_count".to_string())
            .or_insert(serde_json::json!(row.sequence_nr));
        obj.insert(
            "sequence_nr".to_string(),
            serde_json::json!(row.sequence_nr),
        );
        obj.insert(
            "@odata.id".to_string(),
            serde_json::json!(format!("{entity_set_name}('{id}')")),
        );
        return state;
    }

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
    match load_catalog_rows_by_id(&query_plane, tenant.as_str(), entity_type, entity_ids).await {
        Ok(CatalogRowsLoad::Available(rows)) => rows,
        Ok(CatalogRowsLoad::Unsupported) => BTreeMap::new(),
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

async fn try_load_selected_catalog_rows(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_ids: &[String],
    selected_fields: &[String],
) -> BTreeMap<String, EntityCatalogRow> {
    let Some(query_plane) = state.query_plane_store() else {
        return BTreeMap::new();
    };
    match load_selected_catalog_rows_by_id(
        &query_plane,
        tenant.as_str(),
        entity_type,
        entity_ids,
        selected_fields,
    )
    .await
    {
        Ok(CatalogRowsLoad::Available(rows)) => rows,
        Ok(CatalogRowsLoad::Unsupported) => {
            try_load_catalog_rows(state, tenant, entity_type, entity_ids).await
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                tenant = %tenant,
                entity_type = %entity_type,
                "selected catalog fast-read failed; falling back to full catalog materialization"
            );
            try_load_catalog_rows(state, tenant, entity_type, entity_ids).await
        }
    }
}

pub(super) async fn missing_catalog_entity_ids(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_ids: &[String],
) -> Vec<String> {
    if entity_ids.is_empty() {
        return Vec::new();
    }
    let Some(query_plane) = state.query_plane_store() else {
        return Vec::new();
    };

    let coverage_fields = [String::from("entity_id")];
    let present_ids = match load_selected_catalog_rows_by_id(
        &query_plane,
        tenant.as_str(),
        entity_type,
        entity_ids,
        &coverage_fields,
    )
    .await
    {
        Ok(CatalogRowsLoad::Available(rows)) => Some(rows.into_keys().collect::<Vec<_>>()),
        Ok(CatalogRowsLoad::Unsupported) => {
            match load_catalog_rows_by_id(&query_plane, tenant.as_str(), entity_type, entity_ids)
                .await
            {
                Ok(CatalogRowsLoad::Available(rows)) => Some(rows.into_keys().collect()),
                Ok(CatalogRowsLoad::Unsupported) => None,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        tenant = %tenant,
                        entity_type = %entity_type,
                        "catalog coverage row check failed; trusting SQL filter push-down result"
                    );
                    None
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                tenant = %tenant,
                entity_type = %entity_type,
                "catalog coverage presence check failed; trusting SQL filter push-down result"
            );
            None
        }
    };

    let Some(present_ids) = present_ids else {
        return Vec::new();
    };
    let present = present_ids.into_iter().collect::<BTreeSet<_>>();
    entity_ids
        .iter()
        .filter(|id| !present.contains(*id))
        .cloned()
        .collect()
}

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
pub(super) async fn try_load_entity_body_from_catalog(
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

pub(super) async fn materialize_entity_set_entities(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_set_name: &str,
    entity_ids: &[String],
    prefer_catalog: bool,
    selected_catalog_fields: Option<&[String]>,
) -> MaterializedEntitySet {
    let selected_catalog_fields_owned = selected_catalog_fields.map(Vec::from);
    let mut catalog_hits: BTreeMap<String, EntityCatalogRow> =
        if should_read_catalog_for_materialization(prefer_catalog) {
            match selected_catalog_fields {
                Some(select) => {
                    try_load_selected_catalog_rows(state, tenant, entity_type, entity_ids, select)
                        .await
                }
                None => try_load_catalog_rows(state, tenant, entity_type, entity_ids).await,
            }
        } else {
            BTreeMap::new()
        };
    let mut shadow_budget = CatalogShadowReadBudget::for_entity_set();

    let concurrency = entity_set_materialization_concurrency();
    let entities = stream::iter(entity_ids.iter().cloned())
        .map(|id| {
            let catalog_row = catalog_hits.remove(&id);
            if selected_catalog_fields_owned.is_none()
                && let Some(row) = catalog_row.as_ref()
            {
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
            let selected_catalog_fields = selected_catalog_fields_owned.clone();
            async move {
                if let Some(row) = catalog_row {
                    let entity = match selected_catalog_fields.as_deref() {
                        Some(select) => catalog_row_to_selected_entity_body(
                            &entity_type,
                            &entity_set_name,
                            row,
                            select,
                        ),
                        None => catalog_row_to_entity_body(&entity_type, &entity_set_name, row),
                    };
                    return Some(entity);
                }
                match crate::application_data::GovernedApplicationDataService::new(&state)
                    .get(&tenant, &entity_type, &id)
                    .await
                {
                    Ok(response) => {
                        if response.state.status != "Deleted"
                            && let Some(query_plane) = state.query_plane_store()
                        {
                            let fields = state.query_projection_fields(
                                &tenant,
                                &entity_type,
                                &response.state.fields,
                            );
                            let projected_state = state.query_projection_state(&response.state);
                            if let Err(error) = query_plane
                                .upsert_projection(
                                    tenant.as_str(),
                                    &entity_type,
                                    &id,
                                    &response.state.status,
                                    &fields,
                                    &projected_state,
                                    response.state.sequence_nr,
                                )
                                .await
                            {
                                tracing::debug!(
                                    error = %error,
                                    tenant = %tenant,
                                    entity_type = %entity_type,
                                    entity_id = %id,
                                    "failed to repair query projection after actor materialization fallback"
                                );
                            }
                        }
                        let mut entity = serde_json::to_value(&response.state).unwrap_or_default();
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

#[cfg(test)]
#[derive(Debug)]
pub(super) struct SelectedEntityIdsForMaterialization {
    pub(super) entity_ids: Vec<String>,
    pub(super) apply_options: temper_odata::query::types::QueryOptions,
    pub(super) precomputed_count: Option<usize>,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
pub(super) struct EntitySelectionTooLarge {
    pub(super) candidate_count: usize,
    pub(super) candidate_budget: usize,
}

#[cfg(test)]
pub(super) fn select_entity_ids_for_materialization(
    mut entity_ids: Vec<String>,
    query_options: &temper_odata::query::types::QueryOptions,
    default_page_size: usize,
    max_entities: usize,
    has_row_authorization: bool,
) -> Result<SelectedEntityIdsForMaterialization, EntitySelectionTooLarge> {
    let has_filter_or_order =
        query_options.filter.is_some() || query_options.orderby.is_some() || has_row_authorization;
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
        // Safety cap: impose a hard ceiling (10× max_entities) and reject
        // reads that cannot be proven complete inside the candidate budget.
        let safety_cap = max_entities.saturating_mul(10);
        if entity_ids.len() > safety_cap {
            return Err(EntitySelectionTooLarge {
                candidate_count: entity_ids.len(),
                candidate_budget: safety_cap,
            });
        }

        let mut adjusted = query_options.clone();
        if adjusted.top.is_none() {
            adjusted.top = Some(default_page_size);
        } else if let Some(top) = adjusted.top {
            adjusted.top = Some(top.min(max_entities));
        }
        adjusted
    };

    Ok(SelectedEntityIdsForMaterialization {
        entity_ids,
        apply_options,
        precomputed_count,
    })
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
