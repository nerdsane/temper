//! Deterministic key-ownership reconciliation and coverage fencing.

use std::collections::BTreeSet;

use temper_runtime::persistence::{EntityKeyLookup, EntityKeyRow, PersistenceError};
use temper_runtime::tenant::parse_persistence_id_parts;

use super::SimEventStoreInner;

const UNKNOWN_KEY_SET_SIGNATURE: &str = "<unknown>";

pub(super) fn reconcile_key_contract_locked(
    inner: &mut SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
    key_set_signature: Option<&str>,
) -> Result<u64, PersistenceError> {
    let type_key = (tenant.to_string(), entity_type.to_string());
    let supplied = key_set_signature
        .unwrap_or(UNKNOWN_KEY_SET_SIGNATURE)
        .to_string();
    match inner.key_index_contract.get(&type_key).cloned() {
        Some((current, revision)) if current == supplied => Ok(revision),
        Some((_, revision)) => {
            let next = revision.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "SimEventStore: key contract revision overflow for {tenant}:{entity_type}"
                ))
            })?;
            inner
                .key_index_contract
                .insert(type_key.clone(), (supplied, next));
            inner.key_index_watermark.remove(&type_key);
            Ok(next)
        }
        None => {
            inner
                .key_index_contract
                .insert(type_key.clone(), (supplied, 1));
            inner.key_index_watermark.remove(&type_key);
            Ok(1)
        }
    }
}

pub(super) fn backfill_entity_keys(
    inner: &mut SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    expected_sequence: u64,
    key_rows: &[EntityKeyRow],
) -> Result<(), PersistenceError> {
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
    let current_sequence = inner
        .journals
        .get(&persistence_id)
        .and_then(|journal| journal.last())
        .map(|event| event.sequence_nr)
        .unwrap_or(0);
    if current_sequence != expected_sequence {
        return Err(PersistenceError::ConcurrencyViolation {
            expected: expected_sequence,
            actual: current_sequence,
        });
    }

    let mut accepted_rows = Vec::with_capacity(key_rows.len());
    for row in key_rows {
        let slot = (
            tenant.to_string(),
            entity_type.to_string(),
            row.key_name.clone(),
            row.key_hash.clone(),
        );
        if let Some(existing) = inner.key_index.get(&slot)
            && existing.0.as_str() != entity_id
        {
            return Err(PersistenceError::Storage(format!(
                "duplicate declared key '{}' for {entity_type}: held by {}",
                row.key_name, existing.0
            )));
        }
        accepted_rows.push(slot);
    }

    // Exact repair: purge every old row first, including when the current set is
    // empty, then install the fully validated current claims.
    inner.key_index.retain(|(t, et, _, _), (eid, _)| {
        !(t.as_str() == tenant && et.as_str() == entity_type && eid.as_str() == entity_id)
    });
    for slot in accepted_rows {
        inner
            .key_index
            .insert(slot, (entity_id.to_string(), expected_sequence));
    }
    Ok(())
}

pub(super) fn lookup(
    inner: &SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
    key_name: &str,
    key_hash: &str,
) -> Option<EntityKeyLookup> {
    let slot = (
        tenant.to_string(),
        entity_type.to_string(),
        key_name.to_string(),
        key_hash.to_string(),
    );
    inner
        .key_index
        .get(&slot)
        .map(|(entity_id, sequence_nr)| EntityKeyLookup {
            entity_id: entity_id.clone(),
            sequence_nr: *sequence_nr,
        })
}

pub(super) fn mark_backfilled(
    inner: &mut SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
    key_set: &str,
) -> Result<(), PersistenceError> {
    reconcile_key_contract_locked(inner, tenant, entity_type, Some(key_set))?;
    inner.key_index_watermark.insert(
        (tenant.to_string(), entity_type.to_string()),
        key_set.to_string(),
    );
    Ok(())
}

pub(super) fn backfilled_types(inner: &SimEventStoreInner, tenant: &str) -> Vec<(String, String)> {
    inner
        .key_index_watermark
        .iter()
        .filter(|((t, _), _)| t.as_str() == tenant)
        .map(|((_, entity_type), key_set)| (entity_type.clone(), key_set.clone()))
        .collect()
}

pub(super) fn reconciliation_revision(
    inner: &SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
) -> u64 {
    inner
        .key_index_contract
        .get(&(tenant.to_string(), entity_type.to_string()))
        .map(|(_, revision)| *revision)
        .unwrap_or(0)
}

pub(super) fn mark_backfilled_if_revision(
    inner: &mut SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
    key_set: &str,
    expected_revision: u64,
) -> bool {
    let type_key = (tenant.to_string(), entity_type.to_string());
    let current = inner.key_index_contract.get(&type_key).cloned();
    if !matches!(
        current,
        Some((ref current_key_set, revision))
            if current_key_set == key_set && revision == expected_revision
    ) {
        return false;
    }
    inner
        .key_index_watermark
        .insert(type_key, key_set.to_string());
    true
}

pub(super) fn keyed_entity_ids(
    inner: &SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for ((found_tenant, found_type, _, _), (entity_id, _)) in &inner.key_index {
        if found_tenant == tenant && found_type == entity_type {
            ids.insert(entity_id.clone());
        }
    }
    ids.into_iter().collect()
}

pub(super) fn reconciliation_entity_ids(
    inner: &SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
) -> Vec<String> {
    let mut owners = BTreeSet::new();
    for persistence_id in inner.journals.keys() {
        if let Ok((found_tenant, found_type, entity_id)) =
            parse_persistence_id_parts(persistence_id)
            && found_tenant == tenant
            && found_type == entity_type
        {
            owners.insert(entity_id.to_string());
        }
    }
    for ((found_tenant, found_type, _, _), (entity_id, _)) in &inner.key_index {
        if found_tenant == tenant && found_type == entity_type {
            owners.insert(entity_id.clone());
        }
    }
    owners.into_iter().collect()
}
