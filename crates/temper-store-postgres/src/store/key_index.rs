//! PostgreSQL key-ownership reconciliation and coverage fencing.

use sqlx::PgPool;
use temper_runtime::persistence::{EntityKeyRow, KeyIndexBackfillFence, PersistenceError};

mod activation;
mod coverage;
mod query;

pub(super) use activation::{activate_contract, activate_contracts};
pub(crate) use coverage::{
    DerivedWriteSource, KeyContractUse, invalidate_key_coverage_for_derived_write,
    invalidate_key_coverage_for_unreconciled_append, reconcile_key_contract_state,
};
pub(super) use query::{keyed_entity_ids, lookup};

/// Serialize every journal mutation and sequence-fenced derived-index repair for one
/// persistence stream. Advisory transaction locks avoid a new coordination table;
/// batch appends acquire them in sorted order to prevent deadlocks.
pub(crate) async fn lock_event_stream(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    persistence_id: &str,
) -> Result<(), PersistenceError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(persistence_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(())
}

pub(crate) fn event_stream_lock_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
    format!("{tenant}:{entity_type}:{entity_id}")
}

fn key_contract_lock_key(tenant: &str, entity_type: &str) -> String {
    format!("key-index-contract:{tenant}:{entity_type}")
}

/// Serialize type-wide key-contract revisions with live appends and conditional
/// watermark publication. Callers acquire this before any stream lock.
pub(crate) async fn lock_key_contract(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    entity_type: &str,
) -> Result<(), PersistenceError> {
    lock_event_stream(tx, &key_contract_lock_key(tenant, entity_type)).await
}

pub(super) async fn backfill_entity_keys(
    pool: &PgPool,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    expected_sequence: u64,
    contract_fence: KeyIndexBackfillFence<'_>,
    key_rows: &[EntityKeyRow],
) -> Result<(), PersistenceError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    // Preserve the global lock order used by live appends and batches: the type
    // contract fence is always acquired before an entity stream fence.
    lock_key_contract(&mut tx, tenant, entity_type).await?;
    let current_contract: Option<(String, i64)> = crate::dbm::postgres_query_as!(
        "SELECT key_set, revision FROM key_index_contract_state \
         WHERE tenant = $1 AND entity_type = $2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(entity_type)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let actual_revision = current_contract
        .as_ref()
        .map(|(_, revision)| *revision as u64)
        .unwrap_or(0);
    if !matches!(
        current_contract,
        Some((ref signature, revision))
            if signature == contract_fence.key_set_signature
                && revision as u64 == contract_fence.contract_revision
    ) {
        return Err(PersistenceError::KeyContractChanged {
            expected_signature: contract_fence.key_set_signature.to_string(),
            expected_revision: contract_fence.contract_revision,
            actual_signature: current_contract.map(|(signature, _)| signature),
            actual_revision,
        });
    }

    let lock_key = event_stream_lock_key(tenant, entity_type, entity_id);
    lock_event_stream(&mut tx, &lock_key).await?;

    let current_snapshot: Option<(i64, Vec<u8>)> = crate::dbm::postgres_query_as!(
        "SELECT sequence_nr, state FROM snapshots \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let snapshot_matches = match (contract_fence.expected_snapshot, current_snapshot.as_ref()) {
        (None, None) => true,
        (Some(expected), Some((sequence_nr, state))) => {
            *sequence_nr >= 0
                && *sequence_nr as u64 == expected.sequence_nr
                && state.as_slice() == expected.state
        }
        _ => false,
    };
    if !snapshot_matches {
        return Err(PersistenceError::SnapshotGenerationChanged);
    }

    // Source authority is categorical, not numeric: journal first, then snapshot,
    // then catalog only when neither stronger source exists. A higher compatibility
    // catalog sequence must never outrank an exact snapshot generation.
    let row: (i64, i64, bool) = crate::dbm::postgres_query_as!(
        "SELECT CASE WHEN EXISTS ( \
             SELECT 1 FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
           ) THEN COALESCE(( \
             SELECT MAX(sequence_nr) FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
           ), 0) WHEN EXISTS ( \
             SELECT 1 FROM snapshots \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
           ) THEN COALESCE(( \
               SELECT sequence_nr FROM snapshots \
               WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
             ), 0) ELSE COALESCE(( \
               SELECT sequence_nr FROM entity_catalog \
               WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
             ), 0) \
           END, COALESCE(( \
           SELECT MAX(sequence_nr) FROM events \
           WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
         ), 0), ( \
           ( \
             EXISTS ( \
               SELECT 1 FROM events \
               WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
             ) \
             OR EXISTS ( \
               SELECT 1 FROM snapshots \
               WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
             ) \
             OR EXISTS ( \
               SELECT 1 FROM entity_catalog \
               WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
             ) \
             OR EXISTS ( \
               SELECT 1 FROM entity_field_index \
               WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
             ) \
           ) \
           AND NOT EXISTS ( \
             SELECT 1 FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
               AND (CASE WHEN jsonb_typeof(payload) = 'object' AND payload ? 'to_status' \
                 THEN payload ->> 'to_status' = 'Deleted' \
                 ELSE event_type = 'Deleted' OR payload ->> 'action' = 'Deleted' \
               END) \
           ) \
         )",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let current_sequence = row.0 as u64;
    let current_journal_sequence = row.1 as u64;
    if current_journal_sequence != contract_fence.expected_journal_sequence {
        return Err(PersistenceError::JournalBoundaryChanged {
            expected: contract_fence.expected_journal_sequence,
            actual: current_journal_sequence,
        });
    }
    let current_entity_live = row.2;
    if current_entity_live != contract_fence.expected_entity_live {
        return Err(PersistenceError::EntityLivenessChanged {
            expected_live: contract_fence.expected_entity_live,
            actual_live: current_entity_live,
        });
    }
    if current_sequence != expected_sequence {
        return Err(PersistenceError::ConcurrencyViolation {
            expected: expected_sequence,
            actual: current_sequence,
        });
    }

    for key in key_rows {
        let holder: Option<(String,)> = crate::dbm::postgres_query_as!(
            "SELECT entity_id FROM entity_key_index \
             WHERE tenant = $1 AND entity_type = $2 AND key_name = $3 AND key_hash = $4",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(&key.key_name)
        .bind(&key.key_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        if let Some((existing,)) = holder
            && existing != entity_id
        {
            return Err(PersistenceError::Storage(format!(
                "duplicate declared key '{}' for {entity_type}: held by {existing}",
                key.key_name
            )));
        }
    }

    // Exact reconciliation includes an empty current set, which releases every
    // obsolete claim before the new rows are installed at the fenced sequence.
    crate::dbm::postgres_query!(
        "DELETE FROM entity_key_index \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    for key in key_rows {
        crate::dbm::postgres_query!(
            "INSERT INTO entity_key_index \
             (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(&key.key_name)
        .bind(&key.key_hash)
        .bind(entity_id)
        .bind(expected_sequence as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    }
    tx.commit()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(())
}

async fn upsert_watermark(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    entity_type: &str,
    key_set: &str,
) -> Result<(), PersistenceError> {
    crate::dbm::postgres_query!(
        "INSERT INTO key_index_backfill_watermark (tenant, entity_type, key_set) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (tenant, entity_type) \
         DO UPDATE SET key_set = EXCLUDED.key_set, completed_at = now()",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(key_set)
    .execute(&mut **tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(())
}

pub(super) async fn mark_backfilled(
    pool: &PgPool,
    tenant: &str,
    entity_type: &str,
    key_set: &str,
) -> Result<(), PersistenceError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    lock_key_contract(&mut tx, tenant, entity_type).await?;
    reconcile_key_contract_state(
        &mut tx,
        tenant,
        entity_type,
        Some(key_set),
        None,
        KeyContractUse::Backfill,
    )
    .await?;
    upsert_watermark(&mut tx, tenant, entity_type, key_set).await?;
    tx.commit()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(())
}

pub(super) async fn backfilled_types(
    pool: &PgPool,
    tenant: &str,
) -> Result<Vec<(String, String)>, PersistenceError> {
    let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
        "SELECT entity_type, key_set FROM key_index_backfill_watermark WHERE tenant = $1",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(rows)
}

pub(super) async fn activated_contracts(
    pool: &PgPool,
) -> Result<Vec<(String, String)>, PersistenceError> {
    let rows = crate::dbm::postgres_query_as!(
        "SELECT tenant, entity_type FROM key_index_contract_state \
         WHERE activated_key_set IS NOT NULL ORDER BY tenant, entity_type",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    Ok(rows)
}

pub(super) async fn reconciliation_revision(
    pool: &PgPool,
    tenant: &str,
    entity_type: &str,
) -> Result<u64, PersistenceError> {
    let revision: Option<i64> = crate::dbm::postgres_query_scalar!(
        "SELECT revision FROM key_index_contract_state \
         WHERE tenant = $1 AND entity_type = $2",
    )
    .bind(tenant)
    .bind(entity_type)
    .fetch_optional(pool)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(revision.unwrap_or(0) as u64)
}

pub(super) async fn begin_backfill(
    pool: &PgPool,
    tenant: &str,
    entity_type: &str,
    key_set: &str,
) -> Result<u64, PersistenceError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    lock_key_contract(&mut tx, tenant, entity_type).await?;
    let revision = reconcile_key_contract_state(
        &mut tx,
        tenant,
        entity_type,
        Some(key_set),
        None,
        KeyContractUse::Backfill,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(revision)
}

pub(super) async fn mark_backfilled_if_revision(
    pool: &PgPool,
    tenant: &str,
    entity_type: &str,
    key_set: &str,
    expected_revision: u64,
) -> Result<bool, PersistenceError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    lock_key_contract(&mut tx, tenant, entity_type).await?;
    let current: Option<(String, i64)> = crate::dbm::postgres_query_as!(
        "SELECT key_set, revision FROM key_index_contract_state \
         WHERE tenant = $1 AND entity_type = $2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(entity_type)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    if !matches!(
        current,
        Some((ref current_key_set, revision))
            if current_key_set == key_set && revision as u64 == expected_revision
    ) {
        return Ok(false);
    }
    upsert_watermark(&mut tx, tenant, entity_type, key_set).await?;
    tx.commit()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(true)
}
