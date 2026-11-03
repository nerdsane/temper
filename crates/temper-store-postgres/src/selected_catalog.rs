//! Catalog reads for OData list projections.
//!
//! Full-document reads and `$select` projections share one batch size so a
//! list of a whole entity type cannot pin a pool connection on a single
//! multi-megabyte JSON transfer.

use std::collections::BTreeSet;

use temper_runtime::persistence::{PersistenceError, catalog_state_from_storage};

use crate::PostgresEventStore;
use crate::platform::{PostgresEntityCatalogRow, storage_error};

/// Ids per catalog `ANY($n)` read.
///
/// `temper serve` checks out up to 32 Postgres connections. Each checkout
/// holds this many JSON documents until the query returns, and glibc keeps
/// that high-water mark after the rows are dropped. A 128-row batch of 32KiB
/// documents on 32 connections peaked near 320MiB and still occupied about
/// 150MiB more than the same load on 10 connections once the rows were
/// freed. 32 rows is a quarter of that concurrent payload.
const CATALOG_READ_BATCH: usize = 32;

type CatalogDocumentRow = (
    String,
    String,
    serde_json::Value,
    Option<serde_json::Value>,
    i64,
    Option<String>,
);

impl PostgresEventStore {
    /// Batch-load full entity catalog rows for a list of entity IDs.
    ///
    /// Returns only rows that exist in the catalog. IDs without a row in the
    /// projection are silently omitted from the result, leaving the caller
    /// free to fall back to the actor path on a per-id basis.
    ///
    /// Large id lists are read in batches of [`CATALOG_READ_BATCH`]. Each
    /// row's `state` comes back whole, its fields put back from the `fields`
    /// column ([`catalog_state_from_storage`]).
    pub async fn load_entity_catalog_rows_pg(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Vec<PostgresEntityCatalogRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut rows = Vec::new();
        for chunk in entity_ids.chunks(CATALOG_READ_BATCH) {
            rows.extend(
                self.load_entity_catalog_rows_pg_chunk(tenant, entity_type, chunk)
                    .await?,
            );
        }
        rows.sort_by(|left, right| left.entity_id.cmp(&right.entity_id));
        Ok(rows)
    }

    async fn load_entity_catalog_rows_pg_chunk(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Vec<PostgresEntityCatalogRow>, PersistenceError> {
        let rows: Vec<CatalogDocumentRow> = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT entity_id, status, fields, state, sequence_nr, first_event_at \
             FROM {schema}entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = ANY($3) \
             ORDER BY entity_id",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_ids)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|(entity_id, status, fields, state, seq, first_event_at)| {
                PostgresEntityCatalogRow {
                    entity_id,
                    status,
                    state: {
                        let mut stored =
                            state.map(|stored| catalog_state_from_storage(&fields, stored));
                        if stored.is_none() && first_event_at.is_some() {
                            stored = Some(serde_json::json!({}));
                        }
                        if let Some(object) =
                            stored.as_mut().and_then(serde_json::Value::as_object_mut)
                            && (first_event_at.is_some() || object.contains_key("first_event_at"))
                        {
                            object
                                .insert("first_event_at".into(), serde_json::json!(first_event_at));
                        }
                        stored
                    },
                    fields,
                    sequence_nr: seq.max(0) as u64,
                }
            })
            .collect())
    }
}

impl PostgresEventStore {
    /// Batch-load catalog rows while projecting only the requested JSON fields.
    ///
    /// This keeps safe OData `$select` list reads from pulling the full
    /// `entity_catalog.state` and `entity_catalog.fields` JSONB payloads over
    /// the database boundary. The selected values are returned in the row's
    /// `fields` object and `state` is intentionally omitted. Selecting `fields`
    /// restores the complete stored object with the same state-over-column
    /// precedence as [`catalog_state_from_storage`].
    pub async fn load_selected_entity_catalog_rows_pg(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        selected_fields: &[String],
    ) -> Result<Vec<PostgresEntityCatalogRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let selected_fields = selected_fields
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        for chunk in entity_ids.chunks(CATALOG_READ_BATCH) {
            rows.extend(
                self.load_selected_entity_catalog_chunk(
                    tenant,
                    entity_type,
                    chunk,
                    &selected_fields,
                )
                .await?,
            );
        }
        rows.sort_by(|left, right| left.entity_id.cmp(&right.entity_id));
        Ok(rows)
    }

    async fn load_selected_entity_catalog_chunk(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        selected_fields: &[String],
    ) -> Result<Vec<PostgresEntityCatalogRow>, PersistenceError> {
        let rows: Vec<(String, String, serde_json::Value, i64)> = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT c.entity_id, \
                    c.status, \
                    COALESCE( \
                        ( \
                            SELECT jsonb_object_agg(k.field_name, selected.value) \
                            FROM unnest($4::text[]) AS k(field_name) \
                            CROSS JOIN LATERAL ( \
                                SELECT CASE WHEN k.field_name = 'fields' THEN \
                                    CASE WHEN jsonb_typeof(c.fields) = 'object' \
                                              AND jsonb_typeof(c.state -> 'fields') = 'object' \
                                         THEN c.fields || (c.state -> 'fields') \
                                         ELSE COALESCE(c.state -> 'fields', c.fields) END \
                                ELSE COALESCE( \
                                    c.state -> k.field_name, \
                                    c.state -> 'fields' -> k.field_name, \
                                    c.fields -> k.field_name \
                                ) END AS value \
                            ) selected \
                            WHERE selected.value IS NOT NULL \
                        ), \
                        '{}'::jsonb \
                    ) AS fields, \
                    c.sequence_nr \
             FROM {schema}entity_catalog c \
             WHERE c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = ANY($3) \
             ORDER BY c.entity_id",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_ids)
        .bind(selected_fields)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;

        Ok(rows
            .into_iter()
            .map(
                |(entity_id, status, fields, seq)| PostgresEntityCatalogRow {
                    entity_id,
                    status,
                    fields,
                    state: None,
                    sequence_nr: seq.max(0) as u64,
                },
            )
            .collect())
    }
}
