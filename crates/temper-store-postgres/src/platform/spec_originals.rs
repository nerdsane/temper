//! The source each spec was stored with before a boot converted it to the
//! current syntax, so the conversion can be undone.

use sqlx::Row;
use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::PostgresEventStore;

impl PostgresEventStore {
    /// Keep `ioa_source` as the source `entity_type` was stored with before
    /// its conversion. Only the first is kept: once one is, this changes nothing.
    pub async fn keep_spec_original(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "INSERT INTO {schema}spec_originals (tenant, entity_type, ioa_source) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (tenant, entity_type) DO NOTHING",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(ioa_source)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Every kept original, as `(tenant, entity_type, ioa_source)`.
    pub async fn load_spec_originals(
        &self,
    ) -> Result<Vec<(String, String, String)>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            &self.schema,
            "SELECT tenant, entity_type, ioa_source FROM {schema}spec_originals \
             ORDER BY tenant, entity_type",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get("tenant"),
                    row.get("entity_type"),
                    row.get("ioa_source"),
                )
            })
            .collect())
    }
}
