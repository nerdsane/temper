//! What the newest review of each app's modules said.

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::TursoEventStore;
use super::write_gate::WritePriority;
use crate::driver::params;

impl TursoEventStore {
    /// Keep what the newest review of `app_name`'s modules said, in place of the last.
    pub async fn save_module_review(
        &self,
        tenant: &str,
        app_name: &str,
        version: &str,
        result: &str,
    ) -> Result<(), PersistenceError> {
        // Written behind the install, which has answered by now.
        let conn = self
            .write_connection("turso.save_module_review", WritePriority::Low)
            .await?;
        conn.execute(
            "INSERT INTO module_reviews (tenant, app_name, version, result, updated_at) \
             VALUES (?1, ?2, ?3, ?4, datetime('now')) \
             ON CONFLICT (tenant, app_name) DO UPDATE SET \
                 version = excluded.version, result = excluded.result, updated_at = excluded.updated_at",
            params![tenant, app_name, version, result],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// The version and result of the newest review of `app_name`'s modules, if one is kept.
    pub async fn load_module_review(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<(String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT version, result FROM module_reviews WHERE tenant = ?1 AND app_name = ?2",
                params![tenant, app_name],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };
        Ok(Some((
            row.get::<String>(0).map_err(storage_error)?,
            row.get::<String>(1).map_err(storage_error)?,
        )))
    }
}
