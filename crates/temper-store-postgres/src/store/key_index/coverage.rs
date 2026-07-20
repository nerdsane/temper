//! Type-wide key-coverage contract and durable-source invalidation.

use temper_runtime::persistence::PersistenceError;

const UNKNOWN_KEY_SET_SIGNATURE: &str = "<unknown>";

/// Identifies the durable derived-state writer that can invalidate coverage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DerivedWriteSource {
    /// Snapshot bytes contribute the recovery baseline even when the journal has
    /// reached the same or a later sequence.
    Snapshot,
    /// Query-plane catalog state is compatibility-only once an equal-or-newer
    /// journal sequence exists.
    Catalog {
        /// Sequence represented by the catalog mutation, when one is known.
        durable_sequence: Option<u64>,
    },
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

/// Advance the current reconciliation epoch when a changed durable snapshot or
/// non-journal-dominated catalog mutation can alter ownership recovery.
///
/// Callers hold the type contract lock and the entity stream lock. The signature
/// remains unchanged; only the monotonic revision advances. Snapshot bytes are a
/// semantic recovery baseline and therefore always invalidate when changed. A
/// catalog mutation at or below the journal high-water is compatibility-only and
/// can reuse the append fence.
pub(crate) async fn invalidate_key_coverage_for_derived_write(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    source: DerivedWriteSource,
) -> Result<(), PersistenceError> {
    if let DerivedWriteSource::Catalog {
        durable_sequence: Some(durable_sequence),
    } = source
    {
        let journal_sequence: Option<i64> = crate::dbm::postgres_query_scalar!(
            "SELECT MAX(sequence_nr) FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        if journal_sequence.is_some_and(|sequence| sequence as u64 >= durable_sequence) {
            return Ok(());
        }
    }

    let current: Option<(String, i64)> = crate::dbm::postgres_query_as!(
        "SELECT key_set, revision FROM key_index_contract_state \
         WHERE tenant = $1 AND entity_type = $2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(entity_type)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let Some((_, revision)) = current else {
        return Ok(());
    };
    let next = revision.checked_add(1).ok_or_else(|| {
        PersistenceError::Storage(format!(
            "key reconciliation revision overflow for {tenant}:{entity_type}"
        ))
    })?;
    crate::dbm::postgres_query!(
        "UPDATE key_index_contract_state \
         SET revision = $3, updated_at = now() \
         WHERE tenant = $1 AND entity_type = $2",
    )
    .bind(tenant)
    .bind(entity_type)
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
    Ok(())
}
