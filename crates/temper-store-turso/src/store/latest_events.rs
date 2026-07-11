//! Bounded latest-event reads for entity liveness classification.

use libsql::params;
use temper_runtime::persistence::{
    EventMetadata, PersistenceEnvelope, PersistenceError, storage_error,
    validate_latest_event_batch,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use super::TursoEventStore;

pub(crate) async fn read_latest_events(
    store: &TursoEventStore,
    persistence_ids: &[String],
) -> Result<Vec<Option<PersistenceEnvelope>>, PersistenceError> {
    validate_latest_event_batch(persistence_ids)?;
    if persistence_ids.is_empty() {
        return Ok(Vec::new());
    }

    let requested = persistence_ids
        .iter()
        .map(|persistence_id| {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
            Ok(serde_json::json!({
                "tenant": tenant,
                "entity_type": entity_type,
                "entity_id": entity_id,
            }))
        })
        .collect::<Result<Vec<_>, PersistenceError>>()?;
    let requested_json = serde_json::to_string(&requested)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;

    let conn = store.configured_connection().await?;
    let mut rows = conn
        .query(
            "WITH requested AS (
               SELECT CAST(key AS INTEGER) AS ordinal,
                      json_extract(value, '$.tenant') AS tenant,
                      json_extract(value, '$.entity_type') AS entity_type,
                      json_extract(value, '$.entity_id') AS entity_id
               FROM json_each(?1)
             ), ranked AS (
               SELECT r.ordinal, e.sequence_nr, e.event_type, e.payload, e.metadata,
                      ROW_NUMBER() OVER (
                        PARTITION BY r.ordinal ORDER BY e.sequence_nr DESC
                      ) AS event_rank
               FROM requested r
               LEFT JOIN events e
                 ON e.tenant = r.tenant
                AND e.entity_type = r.entity_type
                AND e.entity_id = r.entity_id
             )
             SELECT sequence_nr, event_type, payload, metadata
             FROM ranked
             WHERE event_rank = 1
             ORDER BY ordinal",
            params![requested_json],
        )
        .await
        .map_err(storage_error)?;

    let mut out = Vec::with_capacity(persistence_ids.len());
    while let Some(row) = rows.next().await.map_err(storage_error)? {
        let sequence_nr = row.get::<Option<i64>>(0).map_err(storage_error)?;
        let event_type = row.get::<Option<String>>(1).map_err(storage_error)?;
        let payload = row.get::<Option<String>>(2).map_err(storage_error)?;
        let metadata = row.get::<Option<String>>(3).map_err(storage_error)?;
        let event = match (sequence_nr, event_type, payload, metadata) {
            (None, None, None, None) => None,
            (Some(sequence_nr), Some(event_type), Some(payload), Some(metadata)) => {
                let sequence_nr = u64::try_from(sequence_nr).map_err(|_| {
                    PersistenceError::Serialization(
                        "latest event row has a negative sequence".to_string(),
                    )
                })?;
                let payload = serde_json::from_str(&payload)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                let metadata: EventMetadata = serde_json::from_str(&metadata)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                Some(PersistenceEnvelope {
                    sequence_nr,
                    event_type,
                    payload,
                    metadata,
                })
            }
            _ => {
                return Err(PersistenceError::Serialization(
                    "latest event row contains a partial envelope".to_string(),
                ));
            }
        };
        out.push(event);
    }

    if out.len() != persistence_ids.len() {
        return Err(PersistenceError::Storage(format!(
            "latest-event query returned {} rows for {} streams",
            out.len(),
            persistence_ids.len()
        )));
    }
    Ok(out)
}
