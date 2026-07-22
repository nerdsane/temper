//! Deterministic key-ownership reconciliation and coverage fencing.

use std::collections::BTreeSet;

use temper_runtime::persistence::{
    EntityKeyLookup, EntityKeyRow, KeyIndexBackfillFence, PersistenceEnvelope, PersistenceError,
    decode_activated_key_contract, encode_activated_key_contract,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use super::SimEventStoreInner;

const UNKNOWN_KEY_SET_SIGNATURE: &str = "<unknown>";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KeyContractUse {
    LiveWrite,
    Backfill,
}

pub(super) fn reconcile_key_contract_locked(
    inner: &mut SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
    key_set_signature: Option<&str>,
    contract_use: KeyContractUse,
) -> Result<u64, PersistenceError> {
    let type_key = (tenant.to_string(), entity_type.to_string());
    let raw_contract = key_set_signature.unwrap_or(UNKNOWN_KEY_SET_SIGNATURE);
    let (supplied, attempted_epoch) = decode_activated_key_contract(raw_contract);
    let supplied = supplied.to_string();
    match (
        contract_use,
        inner.key_index_activated_contract.get(&type_key),
    ) {
        (KeyContractUse::LiveWrite | KeyContractUse::Backfill, Some((active, _, _)))
            if active != &supplied =>
        {
            return Err(PersistenceError::KeyContractNotActive {
                activated_signature: active.clone(),
                attempted_signature: supplied,
            });
        }
        (KeyContractUse::LiveWrite, Some((_, activated_epoch, _)))
            if attempted_epoch != Some(*activated_epoch) =>
        {
            return Err(PersistenceError::KeyContractActivationStale {
                activated_epoch: *activated_epoch,
                attempted_epoch,
            });
        }
        (KeyContractUse::LiveWrite, Some((active, activated_epoch, _)))
            if inner
                .key_index_watermark
                .get(&type_key)
                .is_none_or(|covered| covered != active) =>
        {
            return Err(PersistenceError::KeyContractActivationNotReady {
                activated_epoch: *activated_epoch,
                activated_signature: active.clone(),
            });
        }
        // Legacy tables and backfill passes do not establish an activation
        // epoch. Epoch enforcement starts only when the runtime explicitly
        // activates a durable spec contract.
        _ => {}
    }
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

pub(super) fn activate_key_contract_locked(
    inner: &mut SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
    key_set_signature: &str,
    spec_fingerprint: &str,
    purge_existing_rows: bool,
) -> Result<u64, PersistenceError> {
    let (key_set_signature, _) = decode_activated_key_contract(key_set_signature);
    let type_key = (tenant.to_string(), entity_type.to_string());
    let activation_epoch = inner
        .key_index_activated_contract
        .get(&type_key)
        .map(|(_, epoch, _)| *epoch)
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| {
            PersistenceError::Storage(format!(
                "SimEventStore: key activation epoch overflow for {tenant}:{entity_type}"
            ))
        })?;
    let prior_activation = inner.key_index_activated_contract.get(&type_key).cloned();
    let semantic_contract_changed =
        prior_activation
            .as_ref()
            .is_none_or(|(active_key_set, _, active_fingerprint)| {
                active_key_set != key_set_signature || active_fingerprint != spec_fingerprint
            });
    let current_key_set = inner.key_index_contract.get(&type_key).cloned();
    inner.key_index_activated_contract.insert(
        type_key.clone(),
        (
            key_set_signature.to_string(),
            activation_epoch,
            spec_fingerprint.to_string(),
        ),
    );
    let activated_contract = encode_activated_key_contract(key_set_signature, activation_epoch);
    reconcile_key_contract_locked(
        inner,
        tenant,
        entity_type,
        Some(&activated_contract),
        KeyContractUse::Backfill,
    )?;
    if semantic_contract_changed
        && current_key_set
            .as_ref()
            .is_some_and(|(signature, _)| signature == key_set_signature)
    {
        let (_, revision) = inner
            .key_index_contract
            .get(&type_key)
            .cloned()
            .expect("activation reconciliation installed a contract");
        let next = revision.checked_add(1).ok_or_else(|| {
            PersistenceError::Storage(format!(
                "SimEventStore: key contract revision overflow for {tenant}:{entity_type}"
            ))
        })?;
        inner
            .key_index_contract
            .insert(type_key.clone(), (key_set_signature.to_string(), next));
        inner.key_index_watermark.remove(&type_key);
    }
    if purge_existing_rows || semantic_contract_changed {
        inner.key_index.retain(|(row_tenant, row_type, _, _), _| {
            row_tenant != tenant || row_type != entity_type
        });
    }
    Ok(activation_epoch)
}

/// Invalidate an in-flight coverage proof when a snapshot mutation changes the
/// durable state used to derive ownership.
///
/// Even at or below the journal high-water, snapshot bytes are the recovery
/// baseline for legacy fields. Keeping the current signature while advancing its
/// revision makes publication ABA-safe without turning a snapshot change into a
/// contract change. Identical snapshot writes are filtered by the caller.
pub(super) fn invalidate_coverage_for_snapshot_write_locked(
    inner: &mut SimEventStoreInner,
    persistence_id: &str,
) -> Result<(), PersistenceError> {
    let (tenant, entity_type, _) =
        parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
    let type_key = (tenant.to_string(), entity_type.to_string());
    let Some((signature, revision)) = inner.key_index_contract.get(&type_key).cloned() else {
        return Ok(());
    };
    let next = revision.checked_add(1).ok_or_else(|| {
        PersistenceError::Storage(format!(
            "SimEventStore: key reconciliation revision overflow for {tenant}:{entity_type}"
        ))
    })?;
    inner
        .key_index_contract
        .insert(type_key.clone(), (signature, next));
    inner.key_index_watermark.remove(&type_key);
    Ok(())
}

/// Close an ownership proof before a journal append that does not carry an
/// exact declared-key reconciliation. The revision change makes a racing
/// repair publication lose its compare-and-set after the append commits.
pub(super) fn invalidate_coverage_for_unreconciled_append_locked(
    inner: &mut SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
) -> Result<(), PersistenceError> {
    let type_key = (tenant.to_string(), entity_type.to_string());
    let Some((signature, revision)) = inner.key_index_contract.get(&type_key).cloned() else {
        return Ok(());
    };
    let next = revision.checked_add(1).ok_or_else(|| {
        PersistenceError::Storage(format!(
            "SimEventStore: key reconciliation revision overflow for {tenant}:{entity_type}"
        ))
    })?;
    inner
        .key_index_contract
        .insert(type_key.clone(), (signature, next));
    inner.key_index_watermark.remove(&type_key);
    Ok(())
}

pub(super) fn backfill_entity_keys(
    inner: &mut SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    expected_sequence: u64,
    contract_fence: KeyIndexBackfillFence<'_>,
    key_rows: &[EntityKeyRow],
) -> Result<(), PersistenceError> {
    let type_key = (tenant.to_string(), entity_type.to_string());
    let current_contract = inner.key_index_contract.get(&type_key).cloned();
    let actual_revision = current_contract
        .as_ref()
        .map(|(_, revision)| *revision)
        .unwrap_or(0);
    if !matches!(
        current_contract,
        Some((ref signature, revision))
            if signature == contract_fence.key_set_signature
                && revision == contract_fence.contract_revision
    ) {
        return Err(PersistenceError::KeyContractChanged {
            expected_signature: contract_fence.key_set_signature.to_string(),
            expected_revision: contract_fence.contract_revision,
            actual_signature: current_contract.map(|(signature, _)| signature),
            actual_revision,
        });
    }

    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
    let snapshot_matches = match (
        contract_fence.expected_snapshot,
        inner.snapshots.get(&persistence_id),
    ) {
        (None, None) => true,
        (Some(expected), Some((sequence_nr, state))) => {
            *sequence_nr == expected.sequence_nr && state.as_slice() == expected.state
        }
        _ => false,
    };
    if !snapshot_matches {
        return Err(PersistenceError::SnapshotGenerationChanged);
    }
    let journal_sequence = inner
        .journals
        .get(&persistence_id)
        .and_then(|journal| journal.last())
        .map(|event| event.sequence_nr)
        .unwrap_or(0);
    let snapshot_sequence = inner
        .snapshots
        .get(&persistence_id)
        .map(|(sequence_nr, _)| *sequence_nr)
        .unwrap_or(0);
    let current_sequence = if journal_sequence > 0 {
        journal_sequence
    } else {
        snapshot_sequence
    };
    if journal_sequence != contract_fence.expected_journal_sequence {
        return Err(PersistenceError::JournalBoundaryChanged {
            expected: contract_fence.expected_journal_sequence,
            actual: journal_sequence,
        });
    }
    let current_entity_live = entity_is_live(inner, &persistence_id);
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
    reconcile_key_contract_locked(
        inner,
        tenant,
        entity_type,
        Some(key_set),
        KeyContractUse::Backfill,
    )?;
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

pub(super) fn activated_contracts(inner: &SimEventStoreInner) -> Vec<(String, String)> {
    inner.key_index_activated_contract.keys().cloned().collect()
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

pub(super) fn live_entity_ids(
    inner: &SimEventStoreInner,
    tenant: &str,
    entity_type: &str,
) -> Vec<String> {
    let mut owners = BTreeSet::new();
    for persistence_id in inner.journals.keys().chain(inner.snapshots.keys()) {
        if let Ok((found_tenant, found_type, entity_id)) =
            parse_persistence_id_parts(persistence_id)
            && found_tenant == tenant
            && found_type == entity_type
            && entity_is_live(inner, persistence_id)
        {
            owners.insert(entity_id.to_string());
        }
    }
    owners.into_iter().collect()
}

fn entity_is_live(inner: &SimEventStoreInner, persistence_id: &str) -> bool {
    let has_durable_state =
        inner.journals.contains_key(persistence_id) || inner.snapshots.contains_key(persistence_id);
    let deleted = inner.journals.get(persistence_id).is_some_and(|journal| {
        journal
            .iter()
            .any(PersistenceEnvelope::transitions_to_deleted)
    });
    has_durable_state && !deleted
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
    for persistence_id in inner.snapshots.keys() {
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
