//! Batched restart comparison: healthy records never load an entity body.
use super::TursoEventStore;
use crate::driver::params;
use temper_runtime::persistence::{
    JournalHead, PersistenceError, ProjectionCheckpoint, ProjectionRepairCandidate, storage_error,
};
use tracing::instrument;

/// Every record's journal head, grouped from all of the tenant's events and
/// snapshots, so it costs the tenant's whole history. Only
/// [`TursoEventStore::load_journal_heads`] reads it, for a query plane kept
/// outside this store; the boot comparison reads [`PENDING_HEADS`].
const HEADS: &str = "WITH heads AS (
    SELECT entity_type, entity_id, MAX(sequence_nr) AS sequence_nr FROM (
        SELECT entity_type, entity_id, sequence_nr FROM events WHERE tenant = ?1
        UNION ALL
        SELECT entity_type, entity_id, sequence_nr FROM snapshots INDEXED BY idx_snapshots_checkpoint WHERE tenant = ?1
    ) GROUP BY entity_type, entity_id
) ";

/// The journal head of each record the projection delta holds
/// (`projection_delta.rs`). Every commit that leaves a record's catalog row
/// behind its journal marks the record in the same transaction: an append
/// without the row, a snapshot past it, a removed row. Only a projection write
/// that reached the mark's sequence retires it, and a database from before
/// the delta was marked whole when the delta was installed. So a record whose
/// row is behind is always held. A record whose row is current is held only
/// while the field index cannot vouch for it (no indexed field, or fields
/// wrapped in a legacy `fields` object) or as a deleted record's terminal
/// mark: the delta grows with such records, never with events. Each held
/// record's head is two seeks by its key, never a read of the tenant's events:
/// on one busy tenant, grouping every event and snapshot took 52 s of a start
/// whose rows were all current.
const PENDING_HEADS: &str = "WITH heads AS (
    SELECT p.entity_type, p.entity_id, MAX(
        COALESCE((SELECT MAX(e.sequence_nr) FROM events e INDEXED BY idx_events_entity
            WHERE e.tenant = ?1 AND e.entity_type = p.entity_type AND e.entity_id = p.entity_id), 0),
        COALESCE((SELECT s.sequence_nr FROM snapshots s INDEXED BY idx_snapshots_checkpoint
            WHERE s.tenant = ?1 AND s.entity_type = p.entity_type AND s.entity_id = p.entity_id), 0)
    ) AS sequence_nr
    FROM entity_projection_delta p WHERE p.tenant = ?1
) ";

// A kernel tombstone is terminal; unlike a spec action named Deleted it does
// not need a new catalog row when the old one has already been removed.
const DELETED: &str = concat!(
    "EXISTS (SELECT 1 FROM events d WHERE d.tenant = ?1
    AND d.entity_type = h.entity_type AND d.entity_id = h.entity_id AND ",
    tombstone_sql!(),
    ")"
);

impl TursoEventStore {
    /// Indexed sequence-only check for one cold point read.
    pub async fn journal_sequence_nr(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<u64, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn.query("SELECT MAX(
            COALESCE((SELECT MAX(sequence_nr) FROM events WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3), 0),
            COALESCE((SELECT MAX(sequence_nr) FROM snapshots INDEXED BY idx_snapshots_checkpoint WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3), 0))",
            params![tenant, entity_type, entity_id]).await.map_err(storage_error)?;
        let row = rows.next().await.map_err(storage_error)?.ok_or_else(|| {
            PersistenceError::Storage("journal sequence query returned no row".into())
        })?;
        Ok(row.get::<i64>(0).map_err(storage_error)? as u64)
    }

    /// Last durable sequences, in one read, for a separately supplied query plane.
    #[instrument(skip_all, fields(otel.name = "turso.journal_heads", tenant))]
    pub async fn load_journal_heads(
        &self,
        tenant: &str,
    ) -> Result<Vec<JournalHead>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let sql = format!(
            "{HEADS} SELECT h.entity_type, h.entity_id, h.sequence_nr, {DELETED} FROM heads h ORDER BY h.entity_type, h.entity_id"
        );
        let mut rows = conn
            .query(&sql, params![tenant])
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(JournalHead {
                entity_type: row.get(0).map_err(storage_error)?,
                entity_id: row.get(1).map_err(storage_error)?,
                sequence_nr: row.get::<i64>(2).map_err(storage_error)? as u64,
                deleted: row.get::<i64>(3).map_err(storage_error)? != 0,
            });
        }
        Ok(out)
    }

    /// Only missing/behind rows, compared with the journal in one tenant query.
    /// It compares only the records the projection delta holds
    /// (`PENDING_HEADS`), so a restart of a tenant whose rows are current
    /// reads no journal however long its history.
    /// The narrow counters row can advance an unchanged catalog projection.
    /// Each record's catalog checkpoint is found by its key: the engine
    /// otherwise takes `idx_entity_catalog_status`, which reads every catalog
    /// row of the record's type for each record (`driver/plan_check.rs`). At
    /// 900 records of one type with 100 KB rows that took 20 s, against 38 ms.
    /// The comparison reads indexes only, never a catalog row or a snapshot
    /// (`idx_entity_catalog_checkpoint`, `idx_snapshots_checkpoint`), so its
    /// cost does not grow with the size of records.
    #[instrument(skip_all, fields(otel.name = "turso.lagging_projections", tenant))]
    pub async fn lagging_query_projections(
        &self,
        tenant: &str,
    ) -> Result<Vec<ProjectionRepairCandidate>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let sql = format!("{PENDING_HEADS}
            SELECT h.entity_type, h.entity_id, h.sequence_nr, {DELETED},
                   COALESCE(k.sequence_nr, c.sequence_nr), c.status
            FROM heads h
            LEFT JOIN entity_catalog c INDEXED BY idx_entity_catalog_checkpoint
              ON c.tenant = ?1 AND c.entity_type = h.entity_type AND c.entity_id = h.entity_id
            LEFT JOIN entity_catalog_counters k ON k.tenant = c.tenant AND k.entity_type = c.entity_type AND k.entity_id = c.entity_id
            WHERE COALESCE(k.sequence_nr, c.sequence_nr, 0) < h.sequence_nr
              AND (c.entity_id IS NOT NULL OR NOT {DELETED})
            ORDER BY h.entity_type, h.entity_id");
        let mut rows = conn
            .query(&sql, params![tenant])
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type: String = row.get(0).map_err(storage_error)?;
            let entity_id: String = row.get(1).map_err(storage_error)?;
            let projected: Option<i64> = row.get(4).map_err(storage_error)?;
            let projection = match projected {
                Some(sequence_nr) => Some(ProjectionCheckpoint {
                    entity_type: entity_type.clone(),
                    entity_id: entity_id.clone(),
                    sequence_nr: sequence_nr as u64,
                    status: row.get(5).map_err(storage_error)?,
                }),
                None => None,
            };
            out.push(ProjectionRepairCandidate {
                journal: JournalHead {
                    entity_type,
                    entity_id,
                    sequence_nr: row.get::<i64>(2).map_err(storage_error)? as u64,
                    deleted: row.get::<i64>(3).map_err(storage_error)? != 0,
                },
                projection,
            });
        }
        Ok(out)
    }

    /// Catalog checkpoints only, for comparison with a different event store.
    /// Like the comparison, it reads the checkpoint index, not the rows.
    pub async fn query_projection_checkpoints(
        &self,
        tenant: &str,
    ) -> Result<Vec<ProjectionCheckpoint>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn.query("SELECT c.entity_type, c.entity_id, COALESCE(k.sequence_nr, c.sequence_nr), c.status
            FROM entity_catalog c INDEXED BY idx_entity_catalog_checkpoint LEFT JOIN entity_catalog_counters k
              ON k.tenant = c.tenant AND k.entity_type = c.entity_type AND k.entity_id = c.entity_id
            WHERE c.tenant = ?1 ORDER BY c.entity_type, c.entity_id", params![tenant]).await.map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(ProjectionCheckpoint {
                entity_type: row.get(0).map_err(storage_error)?,
                entity_id: row.get(1).map_err(storage_error)?,
                sequence_nr: row.get::<i64>(2).map_err(storage_error)? as u64,
                status: row.get(3).map_err(storage_error)?,
            });
        }
        Ok(out)
    }
}
