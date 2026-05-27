//! Shared bounded read helpers for query-plane catalog rows.

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_runtime::persistence::PersistenceError;

use super::{EntityCatalogRow, QueryPlaneStore};

/// Typed outcome for a bounded catalog row load.
#[derive(Debug)]
pub(crate) enum CatalogRowsLoad {
    /// The backend supports the read and returned rows keyed by entity ID.
    Available(BTreeMap<String, EntityCatalogRow>),
    /// The backend does not support this catalog read shape.
    Unsupported,
}

fn rows_by_id(rows: Vec<EntityCatalogRow>) -> BTreeMap<String, EntityCatalogRow> {
    rows.into_iter()
        .map(|row| (row.entity_id.clone(), row))
        .collect()
}

/// Load full catalog rows and preserve unsupported reads as a typed outcome.
pub(crate) async fn load_catalog_rows_by_id(
    query_plane: &Arc<dyn QueryPlaneStore>,
    tenant: &str,
    entity_type: &str,
    entity_ids: &[String],
) -> Result<CatalogRowsLoad, PersistenceError> {
    match query_plane
        .load_entity_catalog_rows(tenant, entity_type, entity_ids)
        .await?
    {
        Some(rows) => Ok(CatalogRowsLoad::Available(rows_by_id(rows))),
        None => Ok(CatalogRowsLoad::Unsupported),
    }
}

/// Load selected catalog rows and preserve unsupported reads as a typed outcome.
pub(crate) async fn load_selected_catalog_rows_by_id(
    query_plane: &Arc<dyn QueryPlaneStore>,
    tenant: &str,
    entity_type: &str,
    entity_ids: &[String],
    selected_fields: &[String],
) -> Result<CatalogRowsLoad, PersistenceError> {
    match query_plane
        .load_selected_entity_catalog_rows(tenant, entity_type, entity_ids, selected_fields)
        .await?
    {
        Some(rows) => Ok(CatalogRowsLoad::Available(rows_by_id(rows))),
        None => Ok(CatalogRowsLoad::Unsupported),
    }
}
