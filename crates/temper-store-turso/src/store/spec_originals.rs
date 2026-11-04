//! The source each spec was stored with before a boot converted it to the
//! current syntax, so the conversion can be undone.

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::TursoEventStore;
use super::write_gate::WritePriority;
use crate::driver::params;

impl TursoEventStore {
    /// Keep `ioa_source` as the source `entity_type` was stored with before
    /// its conversion. Only the first is kept: once one is, this changes nothing.
    pub async fn keep_spec_original(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
    ) -> Result<(), PersistenceError> {
        // Written at boot, just before the converted spec it backs.
        let conn = self
            .write_connection("turso.keep_spec_original", WritePriority::High)
            .await?;
        conn.execute(
            "INSERT INTO spec_originals (tenant, entity_type, ioa_source, kept_at) \
             VALUES (?1, ?2, ?3, datetime('now')) \
             ON CONFLICT (tenant, entity_type) DO NOTHING",
            params![tenant, entity_type, ioa_source],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Every kept original, as `(tenant, entity_type, ioa_source)`.
    pub async fn load_spec_originals(
        &self,
    ) -> Result<Vec<(String, String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, ioa_source FROM spec_originals \
                 ORDER BY tenant, entity_type",
                (),
            )
            .await
            .map_err(storage_error)?;
        let mut originals = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            originals.push((
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
                row.get::<String>(2).map_err(storage_error)?,
            ));
        }
        Ok(originals)
    }
}
