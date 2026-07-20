//! Shared helpers for OData read handlers.

use std::collections::{BTreeMap, BTreeSet};

use futures_util::stream::{self, StreamExt};
use temper_runtime::tenant::TenantId;

use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::state::ServerState;
use crate::storage::{
    CatalogRowsLoad, EntityCatalogRow, load_catalog_rows_by_id, load_selected_catalog_rows_by_id,
};

mod config;
mod entity_set;
mod journal_materialization;
mod projection_repair;
mod select_projection;
mod shadow;

use config::{catalog_fast_read_enabled, entity_set_materialization_concurrency};
pub(super) use config::{odata_default_page_size, odata_max_entities};
#[cfg(test)]
use entity_set::select_entity_ids_for_materialization;
pub(super) use entity_set::{record_entity_set_not_found, resolve_entity_set_name};
pub(super) use journal_materialization::durable_source_absent_for_catalog_materialization;
use journal_materialization::materialize_entity;
use projection_repair::remove_deleted_projection;
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
pub(super) fn catalog_row_to_entity_body(
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

/// Controls when collection materialization may trust the asynchronous catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CatalogMaterializationPolicy {
    /// Use the catalog directly when a row is available.
    Any,
    /// Use a catalog row only for the ADR-0077 migration shape with no journal or
    /// snapshot. Any durable entity source outranks the asynchronous catalog.
    JournalAbsentOnly,
}

/// An authoritative collection candidate could not be proved current.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AuthoritativeMaterializationError {
    /// The durable journal source or generation could not be stabilized.
    JournalUnstable,
}

impl CatalogMaterializationPolicy {
    pub(super) fn uses_catalog(self) -> bool {
        true
    }
}

async fn try_load_catalog_rows(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_ids: &[String],
) -> Result<BTreeMap<String, EntityCatalogRow>, temper_runtime::persistence::PersistenceError> {
    let Some(query_plane) = state.query_plane_store() else {
        return Ok(BTreeMap::new());
    };
    match load_catalog_rows_by_id(&query_plane, tenant.as_str(), entity_type, entity_ids).await {
        Ok(CatalogRowsLoad::Available(rows)) => Ok(rows),
        Ok(CatalogRowsLoad::Unsupported) => Ok(BTreeMap::new()),
        Err(error) => {
            tracing::warn!(
                error = %error,
                tenant = %tenant,
                entity_type = %entity_type,
                "catalog fast-read failed"
            );
            Err(error)
        }
    }
}

async fn try_load_selected_catalog_rows(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_ids: &[String],
    selected_fields: &[String],
) -> Result<BTreeMap<String, EntityCatalogRow>, temper_runtime::persistence::PersistenceError> {
    let Some(query_plane) = state.query_plane_store() else {
        return Ok(BTreeMap::new());
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
        Ok(CatalogRowsLoad::Available(rows)) => Ok(rows),
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
    let rows = try_load_catalog_rows(state, tenant, entity_type, &ids)
        .await
        .ok()?;
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
    catalog_policy: CatalogMaterializationPolicy,
    selected_catalog_fields: Option<&[String]>,
) -> MaterializedEntitySet {
    let selected_catalog_fields_owned = selected_catalog_fields.map(Vec::from);
    let catalog_rows = if catalog_policy.uses_catalog() {
        match selected_catalog_fields {
            Some(select) => {
                try_load_selected_catalog_rows(state, tenant, entity_type, entity_ids, select).await
            }
            None => try_load_catalog_rows(state, tenant, entity_type, entity_ids).await,
        }
    } else {
        Ok(BTreeMap::new())
    };
    let catalog_unavailable = catalog_rows.is_err();
    let mut shadow_budget = CatalogShadowReadBudget::for_entity_set();
    let mut catalog_hits: BTreeMap<String, EntityCatalogRow> = catalog_rows.unwrap_or_default();

    let concurrency = entity_set_materialization_concurrency();
    let outcomes = stream::iter(entity_ids.iter().cloned())
        .map(|id| {
            let catalog_row = catalog_hits.remove(&id);
            if catalog_policy == CatalogMaterializationPolicy::Any
                && selected_catalog_fields_owned.is_none()
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
                    let catalog_allowed = match catalog_policy {
                        CatalogMaterializationPolicy::Any => true,
                        CatalogMaterializationPolicy::JournalAbsentOnly => {
                            match durable_source_absent_for_catalog_materialization(
                                &state,
                                &tenant,
                                &entity_type,
                                &id,
                            )
                            .await
                            {
                                Ok(absent) => absent,
                                Err(error) => return Err(error),
                            }
                        }
                    };
                    if !catalog_allowed {
                        return materialize_entity(
                            &state,
                            &tenant,
                            &entity_type,
                            &entity_set_name,
                            &id,
                            catalog_policy,
                        )
                        .await;
                    }
                    let catalog_entity = if row.status == "Deleted" {
                        if catalog_policy == CatalogMaterializationPolicy::Any {
                            remove_deleted_projection(&state, &tenant, &entity_type, &id).await;
                        }
                        None
                    } else {
                        let mut entity = match selected_catalog_fields.as_deref() {
                            Some(select) => catalog_row_to_selected_entity_body(
                                &entity_type,
                                &entity_set_name,
                                row,
                                select,
                            ),
                            None => catalog_row_to_entity_body(&entity_type, &entity_set_name, row),
                        };
                        hydrate_blob_refs_for_tenant(&state, &tenant, &mut entity).await;
                        Some(entity)
                    };
                    if catalog_policy == CatalogMaterializationPolicy::JournalAbsentOnly {
                        match durable_source_absent_for_catalog_materialization(
                            &state,
                            &tenant,
                            &entity_type,
                            &id,
                        )
                        .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                return materialize_entity(
                                    &state,
                                    &tenant,
                                    &entity_type,
                                    &entity_set_name,
                                    &id,
                                    catalog_policy,
                                )
                                .await;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    return Ok(catalog_entity);
                }
                if catalog_unavailable
                    && catalog_policy == CatalogMaterializationPolicy::JournalAbsentOnly
                    && durable_source_absent_for_catalog_materialization(
                        &state,
                        &tenant,
                        &entity_type,
                        &id,
                    )
                    .await?
                {
                    return Err(AuthoritativeMaterializationError::JournalUnstable);
                }
                materialize_entity(
                    &state,
                    &tenant,
                    &entity_type,
                    &entity_set_name,
                    &id,
                    catalog_policy,
                )
                .await
            }
        })
        .buffered(concurrency)
        .collect::<Vec<_>>()
        .await;
    let mut entities = Vec::with_capacity(outcomes.len());
    let mut error = None;
    for outcome in outcomes {
        match outcome {
            Ok(Some(entity)) => entities.push(entity),
            Ok(None) => {}
            Err(materialization_error) => {
                error.get_or_insert(materialization_error);
            }
        }
    }

    MaterializedEntitySet {
        entities,
        error,
        catalog_shadow_check_budget: shadow_budget.configured(),
        catalog_shadow_check_scheduled: shadow_budget.scheduled(),
    }
}

/// Materialized collection candidates plus an authoritative-source failure, if any.
pub(super) struct MaterializedEntitySet {
    /// Successfully materialized live entity bodies.
    pub(super) entities: Vec<serde_json::Value>,
    /// First failure that prevents the partial bodies from being served as complete.
    pub(super) error: Option<AuthoritativeMaterializationError>,
    /// Configured catalog shadow-read budget for telemetry.
    pub(super) catalog_shadow_check_budget: usize,
    /// Catalog shadow reads actually scheduled for telemetry.
    pub(super) catalog_shadow_check_scheduled: usize,
}

#[cfg(test)]
mod tests;
