//! Journal and entity-listing reads.

use super::*;

impl TursoEventStore {
    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.read_events"))]
    pub(super) async fn read_events_impl(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.read_events_with_head(persistence_id, from_sequence)
            .await
            .map(|read| read.events)
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.read_events_with_head"))]
    pub(super) async fn read_events_with_head_impl(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<JournalRead, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;

        let mut rows = conn
            .query(
                "WITH journal_head AS (
                     SELECT COALESCE(MAX(sequence_nr), 0) AS sequence_nr
                     FROM events
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                 ), tail AS (
                     SELECT sequence_nr, event_type, payload, metadata
                     FROM events
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                       AND sequence_nr > ?4
                 )
                 SELECT tail.sequence_nr, tail.event_type, tail.payload, tail.metadata,
                        journal_head.sequence_nr
                 FROM journal_head
                 LEFT JOIN tail ON 1 = 1
                 ORDER BY tail.sequence_nr ASC",
                params![tenant, entity_type, entity_id, from_sequence as i64],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        let mut journal_head_sequence_nr = None;
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let row_head = row
                .get::<i64>(4)
                .map_err(storage_error)?
                .try_into()
                .map_err(|_| {
                    PersistenceError::Storage(format!(
                        "journal head is negative for {persistence_id}"
                    ))
                })?;
            if journal_head_sequence_nr
                .replace(row_head)
                .is_some_and(|head| head != row_head)
            {
                return Err(PersistenceError::Storage(format!(
                    "journal head changed within one read for {persistence_id}"
                )));
            }

            let sequence_nr = row.get::<Option<i64>>(0).map_err(storage_error)?;
            let event_type = row.get::<Option<String>>(1).map_err(storage_error)?;
            let payload_json = row.get::<Option<String>>(2).map_err(storage_error)?;
            let metadata_json = row.get::<Option<String>>(3).map_err(storage_error)?;
            match (sequence_nr, event_type, payload_json, metadata_json) {
                (Some(sequence_nr), Some(event_type), Some(payload_json), Some(metadata_json)) => {
                    let sequence_nr = sequence_nr.try_into().map_err(|_| {
                        PersistenceError::Storage(format!(
                            "journal sequence is negative for {persistence_id}"
                        ))
                    })?;
                    let payload = serde_json::from_str(&payload_json).map_err(|error| {
                        tracing::error!(%error, "failed to deserialize event payload");
                        PersistenceError::Serialization(error.to_string())
                    })?;
                    let metadata: EventMetadata =
                        serde_json::from_str(&metadata_json).map_err(|error| {
                            tracing::error!(%error, "failed to deserialize event metadata");
                            PersistenceError::Serialization(error.to_string())
                        })?;

                    out.push(PersistenceEnvelope {
                        sequence_nr,
                        event_type,
                        payload,
                        metadata,
                    });
                }
                (None, None, None, None) => {}
                _ => {
                    return Err(PersistenceError::Serialization(format!(
                        "journal query returned a partial event row for {persistence_id}"
                    )));
                }
            }
        }

        let journal_head_sequence_nr = journal_head_sequence_nr.ok_or_else(|| {
            PersistenceError::Storage(format!(
                "journal head query returned no row for {persistence_id}"
            ))
        })?;
        Ok(JournalRead {
            events: out,
            journal_head_sequence_nr,
        })
    }

    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_entity_ids"))]
    pub(super) async fn list_entity_ids_impl(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT DISTINCT e.entity_type, e.entity_id
                 FROM events e
                 WHERE e.tenant = ?1
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
                   )",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type = row.get::<String>(0).map_err(storage_error)?;
            let entity_id = row.get::<String>(1).map_err(storage_error)?;
            out.push((entity_type, entity_id));
        }
        Ok(out)
    }

    pub(super) async fn list_entity_ids_by_type_impl(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.list_entity_ids_by_type_from_read_sources(tenant, entity_type)
            .await
    }

    pub(super) async fn list_entity_ids_limited_impl(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.configured_connection().await?;
        let limit = limit.min(i64::MAX as usize) as i64;
        let mut out = Vec::new();

        if let Some(entity_type) = entity_type {
            let mut rows = conn
                .query(
                    "SELECT DISTINCT e.entity_type, e.entity_id
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
                     ORDER BY e.entity_type, e.entity_id
                     LIMIT ?3",
                    params![tenant, entity_type, limit],
                )
                .await
                .map_err(storage_error)?;

            while let Some(row) = rows.next().await.map_err(storage_error)? {
                out.push((
                    row.get::<String>(0).map_err(storage_error)?,
                    row.get::<String>(1).map_err(storage_error)?,
                ));
            }
            return Ok(out);
        }

        let mut rows = conn
            .query(
                "SELECT DISTINCT e.entity_type, e.entity_id
                 FROM events e
                 WHERE e.tenant = ?1
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
                 ORDER BY e.entity_type, e.entity_id
                 LIMIT ?2",
                params![tenant, limit],
            )
            .await
            .map_err(storage_error)?;

        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push((
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
            ));
        }
        Ok(out)
    }
}
