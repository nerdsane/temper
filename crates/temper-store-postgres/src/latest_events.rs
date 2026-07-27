//! Bounded latest-event reads for entity liveness classification.

use std::collections::BTreeMap;

use sqlx::PgPool;
use temper_runtime::persistence::{
    EventMetadata, PersistenceEnvelope, PersistenceError, validate_latest_event_batch,
};
use temper_runtime::tenant::parse_persistence_id_parts;

type EventRow = (
    String,
    String,
    String,
    i64,
    String,
    serde_json::Value,
    serde_json::Value,
);

pub(crate) async fn read_latest_events(
    pool: &PgPool,
    persistence_ids: &[String],
) -> Result<Vec<Option<PersistenceEnvelope>>, PersistenceError> {
    validate_latest_event_batch(persistence_ids)?;
    if persistence_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut tenants = Vec::with_capacity(persistence_ids.len());
    let mut entity_types = Vec::with_capacity(persistence_ids.len());
    let mut entity_ids = Vec::with_capacity(persistence_ids.len());
    for persistence_id in persistence_ids {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        tenants.push(tenant.to_string());
        entity_types.push(entity_type.to_string());
        entity_ids.push(entity_id.to_string());
    }

    let rows: Vec<EventRow> = crate::dbm::postgres_query_as!(
        "SELECT DISTINCT ON (e.tenant, e.entity_type, e.entity_id) \
                e.tenant, e.entity_type, e.entity_id, e.sequence_nr, \
                e.event_type, e.payload, e.metadata \
         FROM events e \
         JOIN UNNEST($1::text[], $2::text[], $3::text[]) \
              AS requested(tenant, entity_type, entity_id) \
           ON requested.tenant = e.tenant \
          AND requested.entity_type = e.entity_type \
          AND requested.entity_id = e.entity_id \
         ORDER BY e.tenant, e.entity_type, e.entity_id, e.sequence_nr DESC",
    )
    .bind(&tenants)
    .bind(&entity_types)
    .bind(&entity_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;

    let mut by_stream = BTreeMap::new();
    for (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata) in rows {
        let sequence_nr = u64::try_from(sequence_nr).map_err(|_| {
            PersistenceError::Serialization(format!(
                "latest event for {tenant}:{entity_type}:{entity_id} has negative sequence"
            ))
        })?;
        let metadata: EventMetadata = serde_json::from_value(metadata)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        by_stream.insert(
            (tenant, entity_type, entity_id),
            PersistenceEnvelope {
                sequence_nr,
                event_type,
                payload,
                metadata,
            },
        );
    }

    Ok(tenants
        .into_iter()
        .zip(entity_types)
        .zip(entity_ids)
        .map(|((tenant, entity_type), entity_id)| {
            by_stream.get(&(tenant, entity_type, entity_id)).cloned()
        })
        .collect())
}
