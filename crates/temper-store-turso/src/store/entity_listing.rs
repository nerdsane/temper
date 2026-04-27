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
    pub(crate) async fn list_entity_ids_by_type_catalog_first(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_id
                 FROM entity_catalog
                 WHERE tenant = ?1 AND entity_type = ?2
                 ORDER BY entity_id",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;

        let mut catalog_ids = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            catalog_ids.push(row.get::<String>(0).map_err(storage_error)?);
        }
        if !catalog_ids.is_empty() {
            return Ok(catalog_ids);
        }

        let mut rows = conn
            .query(
                "SELECT DISTINCT e.entity_id
                 FROM events e
                 WHERE e.tenant = ?1
                   AND e.entity_type = ?2
                   AND NOT EXISTS (
                     SELECT 1
                     FROM events d
                     WHERE d.tenant = e.tenant
                       AND d.entity_type = e.entity_type
                       AND d.entity_id = e.entity_id
                       AND d.event_type = 'Deleted'
                   )
                 ORDER BY e.entity_id",
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
