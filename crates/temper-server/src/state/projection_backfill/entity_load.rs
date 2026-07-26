//! Durable source selection for projection backfill.

use temper_runtime::tenant::TenantId;

use crate::entity_actor::{
    CapturedEntitySnapshot, EntityRecoveryContext, StableEntitySource,
    recover_entity_state_from_stable_sources,
};

/// Outcome of loading one entity's current state for an index backfill (ADR-0153,
/// ADR-0155). Shared by the key and vector backfills so they classify entities the
/// same way — the distinction is the watermark soundness gate.
pub(super) enum EntityLoadOutcome {
    /// Loaded — index it from these fields, fenced at the replayed journal sequence.
    Fields {
        fields: serde_json::Value,
        sequence_nr: u64,
        journal_sequence: u64,
        snapshot: Option<CapturedEntitySnapshot>,
    },
    /// Durably deleted. Tombstone replay is authoritative even if an asynchronous
    /// catalog row still contains the entity's former live fields.
    Deleted {
        sequence_nr: u64,
        journal_sequence: u64,
        snapshot: Option<CapturedEntitySnapshot>,
    },
    /// No replayable events or valid snapshot state. This can be a true key-only
    /// phantom, or a migration-era entity whose catalog is its durable state.
    Missing {
        sequence_nr: u64,
        journal_sequence: u64,
        snapshot: Option<CapturedEntitySnapshot>,
    },
    /// The entity exists (it was enumerated from the durable store) but its current
    /// state could not be loaded — no transition table to replay with, an unreadable
    /// snapshot, or a replay error. Indexing it is impossible, so the type must NOT be
    /// watermarked; otherwise a read would treat a present-but-unindexed entity as
    /// authoritatively covered. This is the soundness gate.
    LoadFailed,
}

/// Load one entity's CURRENT state plus the exact snapshot/journal generation that
/// must remain unchanged until the index row is reconciled.
pub(super) async fn load_entity_current_fields(
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    table: Option<&temper_jit::TransitionTable>,
    store: &crate::storage::BoxedEventStore,
    backend: crate::storage::BackendLabel,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> EntityLoadOutcome {
    let Some(table) = table else {
        return EntityLoadOutcome::LoadFailed;
    };
    let source: StableEntitySource =
        match recover_entity_state_from_stable_sources(EntityRecoveryContext {
            tenant: tenant.as_str(),
            entity_type,
            entity_id,
            table,
            store,
            backend,
            initial_fields: &serde_json::json!({}),
            blob_store,
        })
        .await
        {
            Ok(source) => source,
            Err(_) => return EntityLoadOutcome::LoadFailed,
        };
    let sequence_nr = source.durable_sequence();
    let journal_sequence = source.journal_sequence;
    let replayed_state_materialization = source.replayed_state_materialization;
    let snapshot = source.snapshot;
    match source.state {
        None => EntityLoadOutcome::Missing {
            sequence_nr,
            journal_sequence,
            snapshot,
        },
        Some(state) if state.status == "Deleted" => EntityLoadOutcome::Deleted {
            sequence_nr,
            journal_sequence,
            snapshot,
        },
        Some(state)
            if state.total_event_count == 0
                && snapshot.is_none()
                && !replayed_state_materialization =>
        {
            EntityLoadOutcome::Missing {
                sequence_nr,
                journal_sequence,
                snapshot,
            }
        }
        Some(state) => EntityLoadOutcome::Fields {
            fields: state.fields,
            sequence_nr,
            journal_sequence,
            snapshot,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use temper_jit::TransitionTable;
    use temper_runtime::persistence::{
        COMPOSITE_EVENT_TYPE, CompositeEvent, EventMetadata, EventStore, PersistenceEnvelope,
    };
    use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
    use temper_store_sim::SimEventStore;

    use super::*;
    use crate::entity_actor::{EntityState, state_materialization_envelope};
    use crate::storage::{BackendLabel, BoxedEventStore};

    const KEYED_RECORD_IOA: &str = r#"
[automaton]
name = "KeyedRecord"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "ExternalId"
type = "string"
initial = ""

[[key]]
name = "external_id"
properties = ["ExternalId"]
"#;

    fn composite_audit_envelope(persistence_id: &str, entity_id: &str) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr: 0,
            event_type: COMPOSITE_EVENT_TYPE.to_string(),
            payload: serde_json::to_value(CompositeEvent {
                tenant: "default".to_string(),
                parent_entity_type: "KeyedRecord".to_string(),
                parent_entity_id: entity_id.to_string(),
                parent_action: "Audit".to_string(),
                composite_idempotency_key: format!("audit-{entity_id}"),
                intent_hash: String::new(),
                sub_writes: Vec::new(),
            })
            .expect("serialize composite audit"),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.to_string(),
            },
        }
    }

    #[tokio::test]
    async fn materialized_zero_domain_event_journal_remains_indexable() {
        let (_guard, _clock, _ids) = install_deterministic_context(238);
        let store = SimEventStore::no_faults(238);
        let boxed = BoxedEventStore::new(store.clone());
        let table = TransitionTable::from_ioa_source(KEYED_RECORD_IOA);
        let entity_id = "materialized-key-owner";
        let persistence_id = format!("default:KeyedRecord:{entity_id}");
        let baseline = EntityState {
            entity_type: "KeyedRecord".to_string(),
            entity_id: entity_id.to_string(),
            status: "Active".to_string(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: serde_json::json!({
                "Id": entity_id,
                "Status": "Active",
                "ExternalId": "durable-owner",
            }),
            events: VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        };
        let materialization = state_materialization_envelope(&persistence_id, &baseline, sim_now())
            .expect("serialize state materialization");
        store
            .append(
                &persistence_id,
                0,
                &[
                    materialization,
                    composite_audit_envelope(&persistence_id, entity_id),
                ],
            )
            .await
            .expect("persist materialization and audit");

        let loaded = load_entity_current_fields(
            &TenantId::default(),
            "KeyedRecord",
            entity_id,
            Some(&table),
            &boxed,
            BackendLabel::Sim,
            None,
        )
        .await;

        match loaded {
            EntityLoadOutcome::Fields {
                fields,
                sequence_nr,
                journal_sequence,
                snapshot,
            } => {
                assert_eq!(fields["ExternalId"], "durable-owner");
                assert_eq!(sequence_nr, 2);
                assert_eq!(journal_sequence, 2);
                assert!(snapshot.is_none());
            }
            _ => panic!(
                "a valid state materialization must distinguish an entity from an audit-only phantom"
            ),
        }
    }

    #[tokio::test]
    async fn audit_only_zero_domain_event_journal_remains_missing() {
        let (_guard, _clock, _ids) = install_deterministic_context(239);
        let store = SimEventStore::no_faults(239);
        let boxed = BoxedEventStore::new(store.clone());
        let table = TransitionTable::from_ioa_source(KEYED_RECORD_IOA);
        let entity_id = "audit-only-phantom";
        let persistence_id = format!("default:KeyedRecord:{entity_id}");
        store
            .append(
                &persistence_id,
                0,
                &[composite_audit_envelope(&persistence_id, entity_id)],
            )
            .await
            .expect("persist audit-only journal");

        assert!(matches!(
            load_entity_current_fields(
                &TenantId::default(),
                "KeyedRecord",
                entity_id,
                Some(&table),
                &boxed,
                BackendLabel::Sim,
                None,
            )
            .await,
            EntityLoadOutcome::Missing {
                journal_sequence: 1,
                ..
            }
        ));
    }
}
