//! Which projection catalog rows still match their entity's journal.

use crate::driver::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;

/// Every catalog row of one entity type, with `1` when no journal event is
/// newer than the projected sequence (the catalog row's, or a newer unchanged
/// projection's in `entity_catalog_counters`) and `0` when one is: a committed change
/// whose projection has not landed, or the entity's tombstone. Each `EXISTS`
/// probe is one seek on `idx_events_entity`. The third column is the row's
/// projected fields when they hold a `fields` object, which the field index
/// does not look inside, and `NULL` otherwise.
const CATALOG_FRESHNESS_SQL: &str = "\
SELECT c.entity_id, 1, CASE WHEN json_type(c.fields, '$.fields') = 'object' THEN c.fields END
    FROM entity_catalog c
    LEFT JOIN entity_catalog_counters k
      ON k.tenant = c.tenant AND k.entity_type = c.entity_type AND k.entity_id = c.entity_id
    WHERE c.tenant = ?1
      AND c.entity_type = ?2
      AND NOT EXISTS (
        SELECT 1
        FROM events e
        WHERE e.tenant = c.tenant
          AND e.entity_type = c.entity_type
          AND e.entity_id = c.entity_id
          AND e.sequence_nr > COALESCE(k.sequence_nr, c.sequence_nr)
      )
UNION ALL
SELECT c.entity_id, 0, CASE WHEN json_type(c.fields, '$.fields') = 'object' THEN c.fields END
    FROM entity_catalog c
    LEFT JOIN entity_catalog_counters k
      ON k.tenant = c.tenant AND k.entity_type = c.entity_type AND k.entity_id = c.entity_id
    WHERE c.tenant = ?1
      AND c.entity_type = ?2
      AND EXISTS (
        SELECT 1
        FROM events e
        WHERE e.tenant = c.tenant
          AND e.entity_type = c.entity_type
          AND e.entity_id = c.entity_id
          AND e.sequence_nr > COALESCE(k.sequence_nr, c.sequence_nr)
      )";

impl TursoEventStore {
    /// Every projection catalog row of `entity_type` as
    /// `(entity_id, current, wrapped_fields)`.
    ///
    /// `current` is true when the entity's journal holds no event newer than
    /// the one the row was projected from, so the row and its field index
    /// entries (written in the same transaction) hold the entity's committed
    /// fields. Both sides are read in one statement. `wrapped_fields` is the
    /// row's projected fields when they hold a `fields` object.
    #[instrument(skip_all, fields(
        otel.name = "turso.projection_catalog_freshness",
        tenant, entity_type,
    ))]
    pub async fn projection_catalog_freshness(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<(String, bool, Option<serde_json::Value>)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(CATALOG_FRESHNESS_SQL, params![tenant, entity_type])
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            let current: i64 = row.get(1).map_err(storage_error)?;
            let wrapped: Option<String> = row.get(2).map_err(storage_error)?;
            let wrapped_fields = wrapped
                .map(|raw| serde_json::from_str(&raw))
                .transpose()
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            out.push((entity_id, current != 0, wrapped_fields));
        }
        Ok(out)
    }
}
