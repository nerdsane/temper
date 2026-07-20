//! Entity listing helpers for Turso/libSQL-backed runtime recovery.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;

impl TursoEventStore {
    #[instrument(skip_all, fields(
        tenant,
        entity_type,
        otel.name = "turso.list_entity_ids_by_type"
    ))]
    pub(crate) async fn list_entity_ids_by_type_from_read_sources(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_id
                 FROM (
                   SELECT c.entity_id
                   FROM entity_catalog c
                   WHERE c.tenant = ?1
                     AND c.entity_type = ?2
                     AND NOT EXISTS (
                       SELECT 1
                       FROM events d
                       WHERE d.tenant = c.tenant
                         AND d.entity_type = c.entity_type
                         AND d.entity_id = c.entity_id
                         AND (
                           json_extract(d.payload, '$.to_status') = 'Deleted'
                           OR (
                             d.event_type = 'Deleted'
                             AND json_type(d.payload, '$.to_status') IS NOT 'text'
                           )
                         )
                     )
                   UNION
                   SELECT f.entity_id
                   FROM entity_field_index f
                   WHERE f.tenant = ?1
                     AND f.entity_type = ?2
                     AND NOT EXISTS (
                       SELECT 1
                       FROM events d
                       WHERE d.tenant = f.tenant
                         AND d.entity_type = f.entity_type
                         AND d.entity_id = f.entity_id
                         AND (
                           json_extract(d.payload, '$.to_status') = 'Deleted'
                           OR (
                             d.event_type = 'Deleted'
                             AND json_type(d.payload, '$.to_status') IS NOT 'text'
                           )
                         )
                     )
                   UNION
                   SELECT DISTINCT e.entity_id
                   FROM events e
                   WHERE e.tenant = ?1
                     AND e.entity_type = ?2
                     AND NOT EXISTS (
                       SELECT 1
                       FROM events d
                       WHERE d.tenant = e.tenant
                         AND d.entity_type = e.entity_type
                         AND d.entity_id = e.entity_id
                         AND (
                           json_extract(d.payload, '$.to_status') = 'Deleted'
                           OR (
                             d.event_type = 'Deleted'
                             AND json_type(d.payload, '$.to_status') IS NOT 'text'
                           )
                         )
                     )
                 )
                 ORDER BY entity_id",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }
}
