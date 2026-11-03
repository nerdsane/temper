//! Tenant-wide checkpoint comparison for boot repair.
use crate::{PostgresEventStore, platform::storage_error, schema::tombstone_sql};
use sqlx::Row;
use temper_runtime::persistence::{
    JournalHead, PersistenceError, ProjectionCheckpoint, ProjectionRepairCandidate,
};

const HEADS: &str = "WITH heads AS (
    SELECT entity_type, entity_id, MAX(sequence_nr) AS sequence_nr FROM (
        SELECT entity_type, entity_id, sequence_nr FROM {schema}events WHERE tenant = $1
        UNION ALL
        SELECT entity_type, entity_id, sequence_nr FROM {schema}snapshots WHERE tenant = $1
    ) s GROUP BY entity_type, entity_id
) ";
const DELETED: &str = concat!(
    "EXISTS (SELECT 1 FROM {schema}events d WHERE d.tenant = $1
    AND d.entity_type = h.entity_type AND d.entity_id = h.entity_id AND ",
    tombstone_sql!(),
    ")"
);

impl PostgresEventStore {
    /// Indexed sequence-only check for one cold point read.
    pub async fn journal_sequence_nr(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<u64, PersistenceError> {
        let seq: i64 = crate::dbm::postgres_query_scalar!(&self.schema, "SELECT GREATEST(
            COALESCE((SELECT MAX(sequence_nr) FROM {schema}events WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3), 0),
            COALESCE((SELECT MAX(sequence_nr) FROM {schema}snapshots WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3), 0))")
            .bind(tenant).bind(entity_type).bind(entity_id).fetch_one(self.pool()).await.map_err(storage_error)?;
        Ok(seq as u64)
    }

    /// Last durable sequences for a separately supplied query plane, in one read.
    pub async fn load_journal_heads(
        &self,
        tenant: &str,
    ) -> Result<Vec<JournalHead>, PersistenceError> {
        let sql = format!(
            "{HEADS} SELECT h.entity_type, h.entity_id, h.sequence_nr, {DELETED} FROM heads h ORDER BY h.entity_type, h.entity_id"
        );
        let rows = crate::dbm::postgres_query!(sql.replace("{schema}", self.schema.prefix()))
            .bind(tenant)
            .fetch_all(self.pool())
            .await
            .map_err(storage_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(JournalHead {
                    entity_type: row.try_get(0).map_err(storage_error)?,
                    entity_id: row.try_get(1).map_err(storage_error)?,
                    sequence_nr: row.try_get::<i64, _>(2).map_err(storage_error)? as u64,
                    deleted: row.try_get(3).map_err(storage_error)?,
                })
            })
            .collect()
    }

    /// Only missing/behind catalog rows, compared with the journal in one query.
    pub async fn lagging_query_projections(
        &self,
        tenant: &str,
    ) -> Result<Vec<ProjectionRepairCandidate>, PersistenceError> {
        let sql = format!(
            "{HEADS}
            SELECT h.entity_type, h.entity_id, h.sequence_nr, {DELETED}, c.sequence_nr, c.status
            FROM heads h LEFT JOIN {{schema}}entity_catalog c
              ON c.tenant = $1 AND c.entity_type = h.entity_type AND c.entity_id = h.entity_id
            WHERE COALESCE(c.sequence_nr, 0) < h.sequence_nr
              AND (c.entity_id IS NOT NULL OR NOT {DELETED})
            ORDER BY h.entity_type, h.entity_id"
        );
        let rows = crate::dbm::postgres_query!(sql.replace("{schema}", self.schema.prefix()))
            .bind(tenant)
            .fetch_all(self.pool())
            .await
            .map_err(storage_error)?;
        rows.into_iter()
            .map(|row| {
                let entity_type: String = row.try_get(0).map_err(storage_error)?;
                let entity_id: String = row.try_get(1).map_err(storage_error)?;
                let seq: Option<i64> = row.try_get(4).map_err(storage_error)?;
                let projection = match seq {
                    Some(seq) => Some(ProjectionCheckpoint {
                        entity_type: entity_type.clone(),
                        entity_id: entity_id.clone(),
                        sequence_nr: seq as u64,
                        status: row.try_get(5).map_err(storage_error)?,
                    }),
                    None => None,
                };
                Ok(ProjectionRepairCandidate {
                    journal: JournalHead {
                        entity_type,
                        entity_id,
                        sequence_nr: row.try_get::<i64, _>(2).map_err(storage_error)? as u64,
                        deleted: row.try_get(3).map_err(storage_error)?,
                    },
                    projection,
                })
            })
            .collect()
    }

    /// Catalog checkpoints only, for comparison with a different journal.
    pub async fn query_projection_checkpoints(
        &self,
        tenant: &str,
    ) -> Result<Vec<ProjectionCheckpoint>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(&self.schema, "SELECT entity_type, entity_id, sequence_nr, status FROM {schema}entity_catalog WHERE tenant = $1 ORDER BY entity_type, entity_id")
            .bind(tenant).fetch_all(self.pool()).await.map_err(storage_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(ProjectionCheckpoint {
                    entity_type: row.try_get(0).map_err(storage_error)?,
                    entity_id: row.try_get(1).map_err(storage_error)?,
                    sequence_nr: row.try_get::<i64, _>(2).map_err(storage_error)? as u64,
                    status: row.try_get(3).map_err(storage_error)?,
                })
            })
            .collect()
    }
}
