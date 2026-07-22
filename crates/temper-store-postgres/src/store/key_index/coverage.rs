//! Type-wide key-coverage contract and durable-source invalidation.

use temper_runtime::persistence::{PersistenceError, decode_activated_key_contract};

const UNKNOWN_KEY_SET_SIGNATURE: &str = "<unknown>";

type KeyContractStateRow = (String, i64, Option<String>, Option<String>, i64, bool);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyContractUse {
    LiveWrite,
    Backfill,
    Activation,
}

/// Identifies the durable derived-state writer that can invalidate coverage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DerivedWriteSource {
    /// Snapshot bytes contribute the recovery baseline even when the journal has
    /// reached the same or a later sequence.
    Snapshot,
    /// Query-plane catalog state is authoritative only when neither a journal nor
    /// a snapshot exists for the entity.
    Catalog,
}

/// Record the key signature used by a durable write. A changed or previously
/// unknown contract advances the monotonic revision and invalidates coverage in the
/// same transaction as the journal append.
pub(crate) async fn reconcile_key_contract_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    entity_type: &str,
    key_set_signature: Option<&str>,
    activation_fingerprint: Option<&str>,
    contract_use: KeyContractUse,
) -> Result<u64, PersistenceError> {
    let raw_contract = key_set_signature.unwrap_or(UNKNOWN_KEY_SET_SIGNATURE);
    let (supplied, attempted_epoch) = decode_activated_key_contract(raw_contract);
    let existing: Option<KeyContractStateRow> =
        crate::dbm::postgres_query_as!(
        "SELECT key_set, revision, activated_key_set, activated_spec_fingerprint, activation_epoch, \
                EXISTS(SELECT 1 FROM key_index_backfill_watermark watermark \
                       WHERE watermark.tenant = $1 \
                         AND watermark.entity_type = $2 \
                         AND watermark.key_set = key_index_contract_state.key_set) \
         FROM key_index_contract_state \
         WHERE tenant = $1 AND entity_type = $2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(entity_type)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;

    if let Some((
        current,
        revision,
        prior_activation,
        prior_fingerprint,
        activation_epoch,
        activation_ready,
    )) = existing
    {
        if let Some(active) = prior_activation.as_deref()
            && active != supplied
            && contract_use != KeyContractUse::Activation
        {
            return Err(PersistenceError::KeyContractNotActive {
                activated_signature: active.to_string(),
                attempted_signature: supplied.to_string(),
            });
        }
        if contract_use == KeyContractUse::LiveWrite
            && prior_activation.is_some()
            && attempted_epoch != Some(activation_epoch as u64)
        {
            return Err(PersistenceError::KeyContractActivationStale {
                activated_epoch: activation_epoch as u64,
                attempted_epoch,
            });
        }
        if contract_use == KeyContractUse::LiveWrite
            && prior_activation.is_some()
            && !activation_ready
        {
            return Err(PersistenceError::KeyContractActivationNotReady {
                activated_epoch: activation_epoch as u64,
                activated_signature: prior_activation
                    .clone()
                    .expect("activation presence checked"),
            });
        }
        let activation_fingerprint = activation_fingerprint.unwrap_or(supplied);
        let semantic_contract_changed = contract_use == KeyContractUse::Activation
            && (prior_activation.as_deref() != Some(supplied)
                || prior_fingerprint.as_deref() != Some(activation_fingerprint));
        let (activated_key_set, activated_spec_fingerprint, next_activation_epoch) =
            if contract_use == KeyContractUse::Activation {
                let next = activation_epoch.checked_add(1).ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "key activation epoch overflow for {tenant}:{entity_type}"
                    ))
                })?;
                (
                    Some(supplied.to_string()),
                    Some(activation_fingerprint.to_string()),
                    next,
                )
            } else {
                (
                    prior_activation.clone(),
                    prior_fingerprint.clone(),
                    activation_epoch,
                )
            };
        if current == supplied {
            let next_revision = if semantic_contract_changed {
                revision.checked_add(1).ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "key contract revision overflow for {tenant}:{entity_type}"
                    ))
                })?
            } else {
                revision
            };
            if activated_key_set != prior_activation || next_activation_epoch != activation_epoch {
                crate::dbm::postgres_query!(
                    "UPDATE key_index_contract_state \
                     SET activated_key_set = $3, activated_spec_fingerprint = $4, \
                         activation_epoch = $5, revision = $6, updated_at = now() \
                     WHERE tenant = $1 AND entity_type = $2",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(activated_key_set)
                .bind(activated_spec_fingerprint)
                .bind(next_activation_epoch)
                .bind(next_revision)
                .execute(&mut **tx)
                .await
                .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            }
            if semantic_contract_changed {
                crate::dbm::postgres_query!(
                    "DELETE FROM key_index_backfill_watermark \
                     WHERE tenant = $1 AND entity_type = $2",
                )
                .bind(tenant)
                .bind(entity_type)
                .execute(&mut **tx)
                .await
                .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            }
            return Ok(if contract_use == KeyContractUse::Activation {
                next_activation_epoch as u64
            } else {
                revision as u64
            });
        }
        let next = revision.checked_add(1).ok_or_else(|| {
            PersistenceError::Storage(format!(
                "key contract revision overflow for {tenant}:{entity_type}"
            ))
        })?;
        crate::dbm::postgres_query!(
            "UPDATE key_index_contract_state \
             SET key_set = $3, revision = $4, activated_key_set = $5, \
                 activated_spec_fingerprint = $6, activation_epoch = $7, updated_at = now() \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(supplied)
        .bind(next)
        .bind(activated_key_set)
        .bind(activated_spec_fingerprint)
        .bind(next_activation_epoch)
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
        return Ok(if contract_use == KeyContractUse::Activation {
            next_activation_epoch as u64
        } else {
            next as u64
        });
    }

    let (activated_key_set, activated_spec_fingerprint, activation_epoch) = match contract_use {
        KeyContractUse::Activation => (
            Some(supplied),
            Some(activation_fingerprint.unwrap_or(supplied)),
            1_i64,
        ),
        KeyContractUse::Backfill | KeyContractUse::LiveWrite => (None, None, 0_i64),
    };
    crate::dbm::postgres_query!(
        "INSERT INTO key_index_contract_state \
         (tenant, entity_type, key_set, revision, activated_key_set, \
          activated_spec_fingerprint, activation_epoch) \
         VALUES ($1, $2, $3, 1, $4, $5, $6)",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(supplied)
    .bind(activated_key_set)
    .bind(activated_spec_fingerprint)
    .bind(activation_epoch)
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
/// catalog mutation is compatibility-only whenever a journal or snapshot exists
/// and can reuse that stronger source's fence.
pub(crate) async fn invalidate_key_coverage_for_derived_write(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    source: DerivedWriteSource,
) -> Result<(), PersistenceError> {
    if source == DerivedWriteSource::Catalog {
        let stronger_source_exists: bool = crate::dbm::postgres_query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3) \
             OR EXISTS(SELECT 1 FROM snapshots \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3)",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        if stronger_source_exists {
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

/// An append that does not carry exact declared-key rows cannot preserve an
/// activated ownership proof. Advance the revision and close readiness in the
/// same transaction so a racing repair CAS loses before the journal mutates.
pub(crate) async fn invalidate_key_coverage_for_unreconciled_append(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    entity_type: &str,
) -> Result<(), PersistenceError> {
    let current: Option<(i64,)> = crate::dbm::postgres_query_as!(
        "SELECT revision FROM key_index_contract_state \
         WHERE tenant = $1 AND entity_type = $2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(entity_type)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    let Some((revision,)) = current else {
        return Ok(());
    };
    let next = revision.checked_add(1).ok_or_else(|| {
        PersistenceError::Storage(format!(
            "key reconciliation revision overflow for {tenant}:{entity_type}"
        ))
    })?;
    crate::dbm::postgres_query!(
        "UPDATE key_index_contract_state SET revision = $3, updated_at = now() \
         WHERE tenant = $1 AND entity_type = $2",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(next)
    .execute(&mut **tx)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    crate::dbm::postgres_query!(
        "DELETE FROM key_index_backfill_watermark \
         WHERE tenant = $1 AND entity_type = $2",
    )
    .bind(tenant)
    .bind(entity_type)
    .execute(&mut **tx)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    Ok(())
}
