use sqlx::PgPool;
use temper_runtime::persistence::{KeyContractActivation, PersistenceError};

use super::{KeyContractUse, lock_key_contract, reconcile_key_contract_state};

pub(in crate::store) async fn activate_contract(
    pool: &PgPool,
    tenant: &str,
    entity_type: &str,
    key_set: &str,
    purge_existing_rows: bool,
) -> Result<u64, PersistenceError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    lock_key_contract(&mut tx, tenant, entity_type).await?;
    let prior_activation: Option<(Option<String>, Option<String>)> =
        crate::dbm::postgres_query_as!(
            "SELECT activated_key_set, activated_spec_fingerprint \
             FROM key_index_contract_state \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let semantic_contract_changed = !matches!(
        prior_activation,
        Some((Some(ref active_key_set), Some(ref active_fingerprint)))
            if active_key_set == key_set && active_fingerprint == key_set
    );
    let revision = reconcile_key_contract_state(
        &mut tx,
        tenant,
        entity_type,
        Some(key_set),
        Some(key_set),
        KeyContractUse::Activation,
    )
    .await?;
    if purge_existing_rows || semantic_contract_changed {
        crate::dbm::postgres_query!(
            "DELETE FROM entity_key_index WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    }
    tx.commit()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(revision)
}

pub(in crate::store) async fn activate_contracts(
    pool: &PgPool,
    tenant: &str,
    activations: &[KeyContractActivation],
) -> Result<std::collections::BTreeMap<String, u64>, PersistenceError> {
    let mut ordered = activations.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.entity_type.cmp(&right.entity_type));
    if ordered
        .windows(2)
        .any(|pair| pair[0].entity_type == pair[1].entity_type)
    {
        return Err(PersistenceError::Storage(format!(
            "duplicate key activation in tenant {tenant}"
        )));
    }

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    // Acquire every type lock in deterministic order before mutating any row,
    // making the tenant activation all-or-nothing and deadlock-safe.
    for activation in &ordered {
        lock_key_contract(&mut tx, tenant, &activation.entity_type).await?;
    }
    let mut epochs = std::collections::BTreeMap::new();
    for activation in ordered {
        let prior_activation: Option<(Option<String>, Option<String>)> =
            crate::dbm::postgres_query_as!(
                "SELECT activated_key_set, activated_spec_fingerprint \
                 FROM key_index_contract_state \
                 WHERE tenant = $1 AND entity_type = $2",
            )
            .bind(tenant)
            .bind(&activation.entity_type)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let semantic_contract_changed = !matches!(
            prior_activation,
            Some((Some(ref active_key_set), Some(ref active_fingerprint)))
                if active_key_set == &activation.key_set
                    && active_fingerprint == &activation.spec_fingerprint
        );
        let epoch = reconcile_key_contract_state(
            &mut tx,
            tenant,
            &activation.entity_type,
            Some(&activation.key_set),
            Some(&activation.spec_fingerprint),
            KeyContractUse::Activation,
        )
        .await?;
        if activation.purge_existing_rows || semantic_contract_changed {
            crate::dbm::postgres_query!(
                "DELETE FROM entity_key_index WHERE tenant = $1 AND entity_type = $2",
            )
            .bind(tenant)
            .bind(&activation.entity_type)
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        }
        epochs.insert(activation.entity_type.clone(), epoch);
    }
    tx.commit()
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(epochs)
}
