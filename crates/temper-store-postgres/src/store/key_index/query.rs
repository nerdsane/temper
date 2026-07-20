//! Declared-key ownership queries.

use sqlx::PgPool;
use temper_runtime::persistence::{EntityKeyLookup, PersistenceError};

pub(in crate::store) async fn keyed_entity_ids(
    pool: &PgPool,
    tenant: &str,
    entity_type: &str,
) -> Result<Vec<String>, PersistenceError> {
    let rows: Vec<(String,)> = crate::dbm::postgres_query_as!(
        "SELECT DISTINCT entity_id FROM entity_key_index \
         WHERE tenant = $1 AND entity_type = $2",
    )
    .bind(tenant)
    .bind(entity_type)
    .fetch_all(pool)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(rows.into_iter().map(|(entity_id,)| entity_id).collect())
}

pub(in crate::store) async fn lookup(
    pool: &PgPool,
    tenant: &str,
    entity_type: &str,
    key_name: &str,
    key_hash: &str,
) -> Result<Option<EntityKeyLookup>, PersistenceError> {
    let row: Option<(String, i64)> = crate::dbm::postgres_query_as!(
        "SELECT entity_id, sequence_nr FROM entity_key_index \
         WHERE tenant = $1 AND entity_type = $2 AND key_name = $3 AND key_hash = $4",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(key_name)
    .bind(key_hash)
    .fetch_optional(pool)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(row.map(|(entity_id, sequence_nr)| EntityKeyLookup {
        entity_id,
        sequence_nr: sequence_nr as u64,
    }))
}
