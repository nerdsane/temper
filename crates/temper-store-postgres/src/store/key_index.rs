//! PostgreSQL key-ownership reconciliation and coverage fencing.

use sqlx::PgPool;
use temper_runtime::persistence::{
    EntityKeyLookup, EntityKeyRow, KeyIndexBackfillFence, PersistenceError,
};

const UNKNOWN_KEY_SET_SIGNATURE: &str = "<unknown>";

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

/// Record the key signature used by a durable write. A changed or previously
/// unknown contract advances the monotonic revision and invalidates coverage in the
/// same transaction as the journal append.
pub(crate) async fn reconcile_key_contract_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    entity_type: &str,
    key_set_signature: Option<&str>,
) -> Result<u64, PersistenceError> {
    let supplied = key_set_signature.unwrap_or(UNKNOWN_KEY_SET_SIGNATURE);
    let existing: Option<(String, i64)> = crate::dbm::postgres_query_as!(
        "SELECT key_set, revision FROM key_index_contract_state \
         WHERE tenant = $1 AND entity_type = $2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(entity_type)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;

    if let Some((current, revision)) = existing {
        if current == supplied {
            return Ok(revision as u64);
        }
        let next = revision.checked_add(1).ok_or_else(|| {
            PersistenceError::Storage(format!(
                "key contract revision overflow for {tenant}:{entity_type}"
            ))
        })?;
        crate::dbm::postgres_query!(
            "UPDATE key_index_contract_state \
             SET key_set = $3, revision = $4, updated_at = now() \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(supplied)
        .bind(next)
        .execute(&mut **tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        crate::dbm::postgres_query!(
            "DELETE FROM key_index_backfill_watermark \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .execute(&mut **tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        return Ok(next as u64);
    }

    crate::dbm::postgres_query!(
        "INSERT INTO key_index_contract_state (tenant, entity_type, key_set, revision) \
         VALUES ($1, $2, $3, 1)",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(supplied)
    .execute(&mut **tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    // A legacy watermark without contract state cannot prove that intervening writes
    // used the same definition. Withhold it until one fenced repair completes.
    crate::dbm::postgres_query!(
        "DELETE FROM key_index_backfill_watermark \
         WHERE tenant = $1 AND entity_type = $2",
    )
    .bind(tenant)
    .bind(entity_type)
    .execute(&mut **tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(1)
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

    let row: (i64,) = crate::dbm::postgres_query_as!(
        "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let current_sequence = row.0 as u64;
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
    reconcile_key_contract_state(&mut tx, tenant, entity_type, Some(key_set)).await?;
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
    let revision =
        reconcile_key_contract_state(&mut tx, tenant, entity_type, Some(key_set)).await?;
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

pub(super) async fn keyed_entity_ids(
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

pub(super) async fn lookup(
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
