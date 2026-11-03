//! A durable read of the gap between journal and field projection.
//!
//! Appends mark an entity in their transaction, after the optimistic
//! concurrency check: a refused append leaves no mark, and two appends to one
//! entity meet on the journal's unique key before either reaches its mark.
//! Projection writes retire that mark only through the sequence they applied.
//! Thus a delayed projection can never hide a newer commit. Wrapped legacy
//! fields remain in the gap because the field index does not see inside them.
//! Terminal tombstones remain as exclusion markers, outside the pending index:
//! a delayed old projection cannot resurrect a deleted match or grow the gap
//! forever. Migration 0023 installs and backfills the table in one
//! transaction, so its existence proves completeness. Every writer touches the
//! mark after every other row it locks, so waiting on a mark cannot close a
//! lock cycle.

use sqlx::{Postgres, Transaction};
use temper_runtime::persistence::PersistenceError;

use crate::platform::storage_error;
use crate::schema::tombstone_sql;
use crate::{PostgresEventStore, PostgresSchema};

/// Mark a commit, including a batch append. The mark only moves forward, and
/// a tombstone's mark stays terminal.
pub(crate) async fn mark_pending(
    schema: &PostgresSchema,
    tx: &mut Transaction<'_, Postgres>,
    [tenant, entity_type, entity_id]: [&str; 3],
    sequence_nr: u64,
    terminal: bool,
) -> Result<(), PersistenceError> {
    crate::dbm::postgres_query!(
        schema,
        "INSERT INTO {schema}entity_projection_delta AS m \
         (tenant, entity_type, entity_id, sequence_nr, terminal) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
             sequence_nr = GREATEST(m.sequence_nr, EXCLUDED.sequence_nr), \
             terminal = m.terminal OR EXCLUDED.terminal",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(stored_sequence(sequence_nr))
    .bind(terminal)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}

/// A snapshot-only import commits a sequence the catalog may not cover yet.
/// A snapshot the catalog already covers needs no mark, and leaving one would
/// keep the entity pending with no projection left to retire it.
pub(crate) async fn mark_snapshot(
    schema: &PostgresSchema,
    tx: &mut Transaction<'_, Postgres>,
    [tenant, entity_type, entity_id]: [&str; 3],
    sequence_nr: u64,
) -> Result<(), PersistenceError> {
    crate::dbm::postgres_query!(
        schema,
        "INSERT INTO {schema}entity_projection_delta AS m \
         (tenant, entity_type, entity_id, sequence_nr) \
         SELECT $1, $2, $3, $4 WHERE NOT EXISTS ( \
             SELECT 1 FROM {schema}entity_catalog c \
             WHERE c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = $3 \
               AND c.sequence_nr >= $4) \
         ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
             sequence_nr = GREATEST(m.sequence_nr, EXCLUDED.sequence_nr)",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(stored_sequence(sequence_nr))
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    // READ COMMITTED takes a new snapshot for this statement. The insert can
    // wait for a projection to retire the old mark, then recreate it using
    // coverage from before that projection committed. Recheck after owning the
    // mark, without locking the catalog (writers lock it before the mark).
    // Preserve newer commits, terminal rows and projections the index cannot
    // vouch for, just as `applied` and the backfill do.
    crate::dbm::postgres_query!(
        schema,
        "DELETE FROM {schema}entity_projection_delta m \
         WHERE m.tenant = $1 AND m.entity_type = $2 AND m.entity_id = $3 \
           AND m.sequence_nr <= $4 AND NOT m.terminal \
           AND EXISTS (SELECT 1 FROM {schema}entity_catalog c \
               WHERE c.tenant = m.tenant AND c.entity_type = m.entity_type \
                 AND c.entity_id = m.entity_id AND c.sequence_nr >= m.sequence_nr \
                 AND jsonb_typeof(c.fields -> 'fields') IS DISTINCT FROM 'object') \
           AND EXISTS (SELECT 1 FROM {schema}entity_field_index f \
               WHERE f.tenant = m.tenant AND f.entity_type = m.entity_type \
                 AND f.entity_id = m.entity_id)",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(stored_sequence(sequence_nr))
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}

/// A removed catalog row still needs reconciliation through its journal head.
/// Recording zero would let an older projection erase the gap after removal.
pub(crate) async fn mark_removed(
    schema: &PostgresSchema,
    tx: &mut Transaction<'_, Postgres>,
    [tenant, entity_type, entity_id]: [&str; 3],
) -> Result<(), PersistenceError> {
    crate::dbm::postgres_query!(
        schema,
        concat!(
            "INSERT INTO {schema}entity_projection_delta AS m \
             (tenant, entity_type, entity_id, sequence_nr, terminal) \
             SELECT $1, $2, $3, GREATEST( \
                 COALESCE((SELECT MAX(sequence_nr) FROM {schema}events \
                     WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3), 0), \
                 COALESCE((SELECT sequence_nr FROM {schema}snapshots \
                     WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3), 0)), \
               EXISTS (SELECT 1 FROM {schema}events d \
                 WHERE d.tenant = $1 AND d.entity_type = $2 AND d.entity_id = $3 AND ",
            tombstone_sql!(),
            ") ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
                 sequence_nr = GREATEST(m.sequence_nr, EXCLUDED.sequence_nr), \
                 terminal = m.terminal OR EXCLUDED.terminal"
        ),
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}

/// A projection covers only marks through its own sequence. Wrapped fields
/// cannot be vouched for by the index, even after the projection catches up.
pub(crate) async fn applied(
    schema: &PostgresSchema,
    tx: &mut Transaction<'_, Postgres>,
    key: [&str; 3],
    sequence_nr: u64,
    fields: &serde_json::Value,
) -> Result<(), PersistenceError> {
    if fields
        .get("fields")
        .is_some_and(serde_json::Value::is_object)
    {
        return mark_pending(schema, tx, key, sequence_nr, false).await;
    }
    let [tenant, entity_type, entity_id] = key;
    // An unchanged projection rewrites only the catalog row, leaving a row
    // projected before the field index existed without index entries. Keep
    // that row in the gap until an index backfill or a changed projection
    // actually indexes it.
    crate::dbm::postgres_query!(
        schema,
        "DELETE FROM {schema}entity_projection_delta m \
         WHERE m.tenant = $1 AND m.entity_type = $2 AND m.entity_id = $3 \
           AND m.sequence_nr <= $4 AND NOT m.terminal \
           AND EXISTS (SELECT 1 FROM {schema}entity_field_index f \
               WHERE f.tenant = $1 AND f.entity_type = $2 AND f.entity_id = $3)",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(stored_sequence(sequence_nr))
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}

impl PostgresEventStore {
    /// All indexed string-equality matches plus the durable projection gap,
    /// from one SQL statement and so one snapshot. `Some(budget)` probes at
    /// most `budget + 1` ids; `None` reads every candidate. Each equality is a
    /// seek on `idx_efi_lookup`; the gap is a range of
    /// `idx_projection_delta_pending`. No catalog bodies or whole-type id list
    /// are read. The second vector identifies the candidates that must be
    /// loaded from authoritative state. Pending rows are taken before projected
    /// matches when the budget is hit, so an empty second vector also proves
    /// that the entire gap was empty. Both vectors are sorted.
    pub async fn reconciled_equality_candidates_pg(
        &self,
        tenant: &str,
        entity_type: &str,
        predicates: &[(String, String)],
        probe_budget: Option<usize>,
    ) -> Result<(Vec<String>, Vec<String>), PersistenceError> {
        assert!(!predicates.is_empty());
        let matches: Vec<String> = (0..predicates.len())
            .map(|index| {
                format!(
                    "SELECT entity_id FROM {{schema}}entity_field_index \
                     WHERE tenant = $1 AND entity_type = $2 \
                       AND field_name = ${} AND field_value = ${}",
                    2 * index + 3,
                    2 * index + 4
                )
            })
            .collect();
        let limit = 2 * predicates.len() + 3;
        let sql = format!(
            "WITH matches AS ({}) \
             SELECT entity_id, bool_or(pending) FROM ( \
                 SELECT m.entity_id, false AS pending FROM matches m WHERE NOT EXISTS ( \
                     SELECT 1 FROM {{schema}}entity_projection_delta d \
                     WHERE d.tenant = $1 AND d.entity_type = $2 \
                       AND d.entity_id = m.entity_id AND d.terminal) \
                 UNION ALL SELECT p.entity_id, true AS pending \
                     FROM {{schema}}entity_projection_delta p \
                     WHERE p.tenant = $1 AND p.entity_type = $2 AND NOT p.terminal \
             ) candidates \
             GROUP BY entity_id ORDER BY bool_or(pending) DESC, entity_id LIMIT ${limit}",
            matches.join(" INTERSECT ")
        );
        let mut query =
            crate::dbm::postgres_query_as!(sql.replace("{schema}", self.schema.prefix()))
                .bind(tenant)
                .bind(entity_type);
        for (field, value) in predicates {
            query = query.bind(field.as_str()).bind(value.as_str());
        }
        // A NULL limit is LIMIT ALL: the complete union from this one snapshot.
        // The probe is an optimization, never a ceiling on a valid query.
        let probe_limit =
            probe_budget.map(|budget| i64::try_from(budget.saturating_add(1)).unwrap_or(i64::MAX));
        let rows: Vec<(String, bool)> = query
            .bind(probe_limit)
            .fetch_all(self.pool())
            .await
            .map_err(storage_error)?;
        let mut ids = Vec::with_capacity(rows.len());
        let mut pending = Vec::new();
        for (id, is_pending) in rows {
            if is_pending {
                pending.push(id.clone());
            }
            ids.push(id);
        }
        // Byte order whatever the database collation, as the read path expects.
        ids.sort();
        pending.sort();
        debug_assert!(pending.len() <= ids.len());
        Ok((ids, pending))
    }
}

/// Sequences are stored as BIGINT; one past its range is past every row.
fn stored_sequence(sequence_nr: u64) -> i64 {
    i64::try_from(sequence_nr).unwrap_or(i64::MAX)
}
