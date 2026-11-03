//! A durable read of the gap between journal and field projection.
//!
//! Appends mark an entity in their transaction. Projection writes retire that
//! mark only through the sequence they applied. Thus a delayed projection can
//! never hide a newer commit. Wrapped legacy fields remain in the gap because
//! the field index does not see inside them. Terminal tombstones remain as
//! exclusion markers, indexed separately from pending rows: a delayed old
//! projection cannot resurrect a deleted match or grow the gap forever.
use super::TursoEventStore;
use super::instrumentation::WriteConnection;
use crate::driver::{Transaction, Value, params, params_from_iter};
use temper_runtime::persistence::{PersistenceError, storage_error};

const CREATE_DELTA: &str = "CREATE TABLE entity_projection_delta (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    sequence_nr INTEGER NOT NULL,
    terminal INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant, entity_type, entity_id)
)";

/// Install and backfill together, once. This is the only whole-corpus read:
/// an older database has no durable record of which projections are behind.
/// A crash rolls back the table as well as its backfill, so existence proves
/// completeness. Normal reads never inspect the catalog to find the gap.
pub(super) async fn migrate_delta(conn: &WriteConnection) -> Result<(), PersistenceError> {
    let tx = conn.begin_immediate().await.map_err(storage_error)?;
    let mut rows = tx
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'entity_projection_delta'",
            (),
        )
        .await
        .map_err(storage_error)?;
    let exists = rows.next().await.map_err(storage_error)?.is_some();
    drop(rows);
    if !exists {
        tx.execute(CREATE_DELTA, ()).await.map_err(storage_error)?;
        tx.execute("CREATE INDEX idx_projection_delta_pending ON entity_projection_delta (tenant, entity_type, terminal, entity_id)", ()).await.map_err(storage_error)?;
        let deleted = format!(
            "EXISTS (SELECT 1 FROM events d WHERE d.tenant = h.tenant AND d.entity_type = h.entity_type AND d.entity_id = h.entity_id AND {})",
            tombstone_sql!()
        );
        let backfill = format!("WITH heads AS (
            SELECT tenant, entity_type, entity_id, MAX(sequence_nr) AS sequence_nr FROM (
                SELECT tenant, entity_type, entity_id, sequence_nr FROM events
                UNION ALL SELECT tenant, entity_type, entity_id, sequence_nr FROM snapshots
                UNION ALL SELECT tenant, entity_type, entity_id, sequence_nr FROM entity_catalog
            ) GROUP BY tenant, entity_type, entity_id
        ) INSERT INTO entity_projection_delta (tenant, entity_type, entity_id, sequence_nr, terminal)
        SELECT h.tenant, h.entity_type, h.entity_id, h.sequence_nr, {deleted} FROM heads h
        LEFT JOIN entity_catalog c INDEXED BY idx_entity_catalog_checkpoint
          ON c.tenant = h.tenant AND c.entity_type = h.entity_type AND c.entity_id = h.entity_id
        LEFT JOIN entity_catalog_counters k
          ON k.tenant = h.tenant AND k.entity_type = h.entity_type AND k.entity_id = h.entity_id
        WHERE {deleted} OR c.entity_id IS NULL OR COALESCE(k.sequence_nr, c.sequence_nr) < h.sequence_nr
           OR json_type(c.fields, '$.fields') = 'object'
           OR NOT EXISTS (SELECT 1 FROM entity_field_index f WHERE f.tenant = h.tenant
                AND f.entity_type = h.entity_type AND f.entity_id = h.entity_id)");
        tx.execute(&backfill, ()).await.map_err(storage_error)?;
    }
    tx.commit().await.map_err(storage_error)
}

/// Mark a commit, including a batch append or a snapshot-only import.
pub(super) async fn mark_pending(
    tx: &Transaction<'_>,
    [tenant, entity_type, entity_id]: [&str; 3],
    sequence_nr: u64,
    terminal: bool,
) -> Result<(), PersistenceError> {
    tx.execute("INSERT INTO entity_projection_delta (tenant, entity_type, entity_id, sequence_nr, terminal)
        VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (tenant, entity_type, entity_id)
        DO UPDATE SET sequence_nr = MAX(sequence_nr, excluded.sequence_nr), terminal = MAX(terminal, excluded.terminal)",
        params![tenant, entity_type, entity_id, sequence_nr as i64, i64::from(terminal)]).await.map_err(storage_error)?;
    Ok(())
}

/// A removed catalog row still needs reconciliation through its journal head.
/// Recording zero would let an older projection erase the gap after removal.
pub(super) async fn mark_removed(
    tx: &Transaction<'_>,
    [tenant, entity_type, entity_id]: [&str; 3],
) -> Result<(), PersistenceError> {
    let deleted = format!(
        "EXISTS (SELECT 1 FROM events d WHERE d.tenant = ?1 AND d.entity_type = ?2 AND d.entity_id = ?3 AND {})",
        tombstone_sql!()
    );
    let sql = format!("INSERT INTO entity_projection_delta (tenant, entity_type, entity_id, sequence_nr, terminal)
        SELECT ?1, ?2, ?3, MAX(
            COALESCE((SELECT MAX(sequence_nr) FROM events WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3), 0),
            COALESCE((SELECT sequence_nr FROM snapshots WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3), 0)), {deleted}
        ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET sequence_nr = MAX(sequence_nr, excluded.sequence_nr), terminal = MAX(terminal, excluded.terminal)");
    tx.execute(&sql, params![tenant, entity_type, entity_id])
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// A projection covers only marks through its own sequence. Wrapped fields
/// cannot be vouched for by the index, even after the projection catches up.
pub(super) async fn applied(
    tx: &Transaction<'_>,
    key: [&str; 3],
    sequence_nr: u64,
    fields: &serde_json::Value,
) -> Result<(), PersistenceError> {
    if fields
        .get("fields")
        .is_some_and(serde_json::Value::is_object)
    {
        return mark_pending(tx, key, sequence_nr, false).await;
    }
    let [tenant, entity_type, entity_id] = key;
    // An unchanged legacy projection can advance only its counters, leaving
    // a pre-index row without index entries. Keep that row in the gap until
    // an index backfill or a changed projection actually indexes it.
    tx.execute(
        "DELETE FROM entity_projection_delta INDEXED BY idx_projection_delta_pending WHERE tenant = ?1 AND entity_type = ?2
        AND entity_id = ?3 AND sequence_nr <= ?4 AND terminal = 0
        AND EXISTS (SELECT 1 FROM entity_field_index f INDEXED BY sqlite_autoindex_entity_field_index_1
            WHERE f.tenant = ?1 AND f.entity_type = ?2 AND f.entity_id = ?3)",
        params![tenant, entity_type, entity_id, sequence_nr as i64],
    )
    .await
    .map_err(storage_error)?;
    Ok(())
}

impl TursoEventStore {
    /// All indexed string-equality matches plus the durable projection gap,
    /// from one SQL snapshot. `Some(budget)` probes at most `budget + 1` ids;
    /// `None` streams every candidate into growable vectors. Each equality is a seek
    /// on `idx_efi_lookup`; the gap is an indexed range of pending entities.
    /// No catalog bodies or whole-type id list are read. The second vector
    /// identifies the candidates that must be loaded from authoritative state.
    /// Pending rows are taken before projected matches when the budget is hit,
    /// so an empty second vector also proves that the entire gap was empty.
    pub async fn reconciled_equality_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        predicates: &[(String, String)],
        probe_budget: Option<usize>,
    ) -> Result<(Vec<String>, Vec<String>), PersistenceError> {
        assert!(!predicates.is_empty());
        let mut values = vec![Value::from(tenant), Value::from(entity_type)];
        let mut matches = Vec::new();
        for (field, value) in predicates {
            let field_param = values.len() + 1;
            let value_param = values.len() + 2;
            values.push(Value::from(field.clone()));
            values.push(Value::from(value.clone()));
            matches.push(format!("SELECT entity_id FROM entity_field_index INDEXED BY idx_efi_lookup
                WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?{field_param} AND field_value = ?{value_param}"));
        }
        let limit = values.len() + 1;
        // SQLite LIMIT -1 streams the complete union from this one snapshot.
        // The probe is an optimization, never a ceiling on a valid query.
        values.push(Value::from(probe_budget.map_or(-1, |budget| {
            budget.saturating_add(1).min(i64::MAX as usize) as i64
        })));
        let sql = format!("WITH matches AS ({})
            SELECT entity_id, MAX(pending) FROM (
                SELECT m.entity_id, 0 AS pending FROM matches m WHERE NOT EXISTS (
                    SELECT 1 FROM entity_projection_delta d INDEXED BY idx_projection_delta_pending WHERE d.tenant = ?1 AND d.entity_type = ?2
                        AND d.entity_id = m.entity_id AND d.terminal = 1)
                UNION ALL SELECT entity_id, 1 AS pending FROM entity_projection_delta INDEXED BY idx_projection_delta_pending
                    WHERE tenant = ?1 AND entity_type = ?2 AND terminal = 0
            ) GROUP BY entity_id ORDER BY MAX(pending) DESC, entity_id LIMIT ?{limit}", matches.join(" INTERSECT "));
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(&sql, params_from_iter(values))
            .await
            .map_err(storage_error)?;
        let mut ids = Vec::new();
        let mut pending = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let id: String = row.get(0).map_err(storage_error)?;
            if row.get::<i64>(1).map_err(storage_error)? != 0 {
                pending.push(id.clone());
            }
            ids.push(id);
        }
        ids.sort();
        Ok((ids, pending))
    }
}
