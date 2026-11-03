//! What the newest review of each app's modules said.

use sqlx::Row;
use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::PostgresEventStore;

impl PostgresEventStore {
    /// Keep what the newest review of `app_name`'s modules said, in place of the last.
    pub async fn save_module_review(
        &self,
        tenant: &str,
        app_name: &str,
        version: &str,
        result: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "INSERT INTO {schema}module_reviews (tenant, app_name, version, result, updated_at) \
             VALUES ($1, $2, $3, $4, now()) \
             ON CONFLICT (tenant, app_name) DO UPDATE SET \
                 version = EXCLUDED.version, result = EXCLUDED.result, updated_at = now()",
        )
        .bind(tenant)
        .bind(app_name)
        .bind(version)
        .bind(result)
        .execute(self.pool())
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
        let row = crate::dbm::postgres_query!(
            &self.schema,
            "SELECT version, result FROM {schema}module_reviews WHERE tenant = $1 AND app_name = $2",
        )
        .bind(tenant)
        .bind(app_name)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(|row| (row.get("version"), row.get("result"))))
    }
}
