//! Count-only totals over the durable query-plane catalog.

use std::collections::BTreeMap;

use temper_runtime::persistence::PersistenceError;

use crate::PostgresEventStore;
use crate::platform::storage_error;

impl PostgresEventStore {
    /// Return one tenant's projected entity counts grouped by entity type.
    ///
    /// A delete removes the catalog row, so deleted entities are not counted.
    /// Totals carry no ids, fields or values.
    pub async fn projected_entity_counts_by_type(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, u64>, PersistenceError> {
        let rows: Vec<(String, i64)> = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT entity_type, COUNT(*)::bigint FROM {schema}entity_catalog \
             WHERE tenant = $1 GROUP BY entity_type",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|(entity_type, count)| (entity_type, count.max(0) as u64))
            .collect())
    }
}
