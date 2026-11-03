//! An entity's query-plane rows, written inside a transaction the caller
//! holds: the journal append's, so a move's projection commits with its
//! event, or the projection writer's own.

use temper_runtime::persistence::{PersistenceError, catalog_state_for_storage, storage_error};
use temper_runtime::scheduler::sim_now;

use super::field_index::{canonical_projection_status, indexed_projection_fields};
use super::field_index_rows::sync_field_index;
use super::unchanged_projections::{
    ExistingProjection, drop_event_counters, projection_hash, store_event_counters,
};
use crate::driver::{Transaction, params};

/// One entity's projection, as its events at `sequence_nr` leave it.
pub(super) struct ProjectionWrite<'a> {
    pub(super) tenant: &'a str,
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) status: &'a str,
    pub(super) fields: &'a serde_json::Value,
    pub(super) state: &'a serde_json::Value,
    pub(super) sequence_nr: u64,
}

/// The entity's catalog row in `tx`, if it has one.
pub(super) async fn existing_projection(
    tx: &Transaction<'_>,
    [tenant, entity_type, entity_id]: [&str; 3],
) -> Result<Option<ExistingProjection>, PersistenceError> {
    let mut rows = tx
        .query(
            // The key's index: without it the planner may read every row of
            // the type (driver/plan_check.rs).
            "SELECT c.sequence_nr, c.projection_hash, k.sequence_nr \
             FROM entity_catalog c INDEXED BY sqlite_autoindex_entity_catalog_1 \
             LEFT JOIN entity_catalog_counters k \
               ON k.tenant = c.tenant AND k.entity_type = c.entity_type \
              AND k.entity_id = c.entity_id \
             WHERE c.tenant = ?1 AND c.entity_type = ?2 AND c.entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
    match rows.next().await.map_err(storage_error)? {
        Some(found) => Ok(Some(ExistingProjection::from_row(&found, 0)?)),
        None => Ok(None),
    }
}

/// Write `row` in `tx`: its catalog row, and its field-index rows when its
/// indexed values changed. A row already at a later sequence is kept.
pub(super) async fn write_projection(
    tx: &Transaction<'_>,
    row: &ProjectionWrite<'_>,
) -> Result<(), PersistenceError> {
    let ProjectionWrite {
        tenant,
        entity_type,
        entity_id,
        fields,
        state,
        ..
    } = *row;
    let status = canonical_projection_status(row.status, state);
    let fields_json = serde_json::to_string(fields)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    // The row holds each field once: in `fields`, not again in `state`.
    let stored_state = catalog_state_for_storage(fields, state);
    let new_hash = projection_hash(status, &fields_json, &stored_state);
    let state_json = serde_json::to_string(&stored_state)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    let updated_at = sim_now().to_rfc3339();
    let sequence_nr = i64::try_from(row.sequence_nr).unwrap_or(i64::MAX);

    let existing = existing_projection(tx, [tenant, entity_type, entity_id]).await?;
    if existing
        .as_ref()
        .is_some_and(|existing| existing.sequence_nr() > sequence_nr)
    {
        return Ok(());
    }

    if let Some(existing) = existing
        .as_ref()
        .filter(|existing| existing.projection_hash == new_hash)
    {
        if sequence_nr > existing.sequence_nr() {
            store_event_counters(tx, [tenant, entity_type, entity_id], sequence_nr, state).await?;
        }
        super::projection_delta::applied(
            tx,
            [tenant, entity_type, entity_id],
            row.sequence_nr,
            fields,
        )
        .await?;
        return Ok(());
    }
    if existing
        .as_ref()
        .is_some_and(|existing| existing.counters_sequence_nr.is_some())
    {
        drop_event_counters(tx, [tenant, entity_type, entity_id]).await?;
    }

    tx.execute(
        "INSERT INTO entity_catalog \
         (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr, projection_version, projection_hash, first_event_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 2, ?9, json_extract(?6, '$.first_event_at')) \
         ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
             status = excluded.status, \
             fields = excluded.fields, \
             state = excluded.state, \
             first_event_at = COALESCE(entity_catalog.first_event_at, excluded.first_event_at), \
             updated_at = excluded.updated_at, \
             sequence_nr = excluded.sequence_nr, \
             projection_version = excluded.projection_version, \
             projection_hash = excluded.projection_hash",
        params![
            tenant,
            entity_type,
            entity_id,
            status,
            fields_json.as_str(),
            state_json.as_str(),
            updated_at.as_str(),
            sequence_nr,
            new_hash.as_str(),
        ],
    )
    .await
    .map_err(storage_error)?;

    let indexed = indexed_projection_fields(status, fields);
    sync_field_index(tx, [tenant, entity_type, entity_id], &indexed).await?;
    super::projection_delta::applied(
        tx,
        [tenant, entity_type, entity_id],
        row.sequence_nr,
        fields,
    )
    .await
}

/// Remove an entity's query-plane rows in `tx`.
pub(super) async fn remove_projection(
    tx: &Transaction<'_>,
    [tenant, entity_type, entity_id]: [&str; 3],
) -> Result<(), PersistenceError> {
    tx.execute(
        "DELETE FROM entity_catalog WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
        params![tenant, entity_type, entity_id],
    )
    .await
    .map_err(storage_error)?;
    tx.execute(
        "DELETE FROM entity_field_index WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
        params![tenant, entity_type, entity_id],
    )
    .await
    .map_err(storage_error)?;
    drop_event_counters(tx, [tenant, entity_type, entity_id]).await?;
    super::projection_delta::mark_removed(tx, [tenant, entity_type, entity_id]).await?;
    Ok(())
}
