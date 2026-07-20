//! Forced interleaving proof for the key-contract read fence.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::storage::{BackendLabel, BoxedEventStore};
use temper_runtime::persistence::{
    EntityKeyRow, EventMetadata, IndexReconciliation, PersistenceAppend, PersistenceAppendResult,
    PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};

#[derive(Clone)]
struct ContractChangingLookupStore {
    inner: SimEventStore,
    persistence_id: String,
    fired: Arc<AtomicBool>,
}

impl EventStore for ContractChangingLookupStore {
    fn supports_authoritative_key_index(&self) -> bool {
        true
    }

    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        EventStore::append(&self.inner, persistence_id, expected_sequence, events).await
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        EventStore::append_batch(&self.inner, appends).await
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        EventStore::read_events(&self.inner, persistence_id, from_sequence).await
    }

    async fn read_events_page(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        through_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        EventStore::read_events_page(
            &self.inner,
            persistence_id,
            from_sequence,
            through_sequence,
            limit,
        )
        .await
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        EventStore::save_snapshot(&self.inner, persistence_id, sequence_nr, snapshot).await
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        EventStore::load_snapshot(&self.inner, persistence_id).await
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        EventStore::list_entity_ids(&self.inner, tenant).await
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        EventStore::list_entity_ids_by_type(&self.inner, tenant, entity_type).await
    }

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        if !self.fired.swap(true, Ordering::SeqCst) {
            let sequence = EventStore::read_events(&self.inner, &self.persistence_id, 0)
                .await?
                .last()
                .map(|event| event.sequence_nr)
                .unwrap_or(0);
            let timestamp = sim_now();
            let event = PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "ContractBWrite".to_string(),
                payload: serde_json::json!({
                    "action": "ContractBWrite",
                    "from_status": "Draft",
                    "to_status": "Draft",
                    "timestamp": timestamp,
                    "params": {},
                    "idempotency_key": null,
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp,
                    actor_id: self.persistence_id.clone(),
                },
            };
            EventStore::append_with_index_rows(
                &self.inner,
                &self.persistence_id,
                sequence,
                &[event],
                &[],
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some("v3|10:contract_b[7:OtherId]".to_string()),
                    vectors: false,
                },
            )
            .await?;
        }
        EventStore::lookup_by_key(&self.inner, tenant, entity_type, key_name, key_hash).await
    }

    async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        EventStore::key_index_backfilled_types(&self.inner, tenant).await
    }

    async fn key_index_reconciliation_revision(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<u64, PersistenceError> {
        EventStore::key_index_reconciliation_revision(&self.inner, tenant, entity_type).await
    }
}

/// Coverage A is observed first; the lookup itself then commits a real exact-set
/// write under incompatible contract B. The post-lookup revision fence must force
/// a journal-backed scan, which still returns the entity matching the captured A
/// fields instead of treating B's key-index miss as authoritative absence.
#[tokio::test]
async fn incompatible_contract_write_between_coverage_and_lookup_falls_back_to_scan() {
    let (_guard, _clock, _ids) = install_deterministic_context(246);
    let tenant = TenantId::default();
    let entity_id = "ord-contract-race";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let workspace = "ws-race";
    let path = "/still-matches-a";
    let inner = SimEventStore::no_faults(246);
    let timestamp = sim_now();
    let create = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Create".to_string(),
        payload: serde_json::json!({
            "action": "Create",
            "from_status": "Draft",
            "to_status": "Draft",
            "timestamp": timestamp,
            "params": {"WorkspaceId": workspace, "Path": path},
            "idempotency_key": null,
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: persistence_id.clone(),
        },
    };
    EventStore::append_with_index_rows(
        &inner,
        &persistence_id,
        0,
        &[create],
        &[EntityKeyRow {
            key_name: "ws_path".to_string(),
            key_hash: ws_path_hash(workspace, path),
        }],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("seed contract-A stream and ownership");
    EventStore::save_snapshot(
        &inner,
        &persistence_id,
        1,
        &serde_json::to_vec(&serde_json::json!({
            "entity_type": "Order",
            "entity_id": entity_id,
            "status": "Draft",
            "item_count": 0,
            "fields": {
                "Id": entity_id,
                "Status": "Draft",
                "WorkspaceId": workspace,
                "Path": path,
            },
        }))
        .expect("snapshot serialization"),
    )
    .await
    .expect("seed replayable state");
    EventStore::mark_key_index_backfilled(
        &inner,
        tenant.as_str(),
        "Order",
        ORDER_KEY_SET_SIGNATURE,
    )
    .await
    .expect("mark contract-A coverage");

    let fired = Arc::new(AtomicBool::new(false));
    let racing_store = ContractChangingLookupStore {
        inner: inner.clone(),
        persistence_id,
        fired: fired.clone(),
    };
    let mut state = build_order_state("query-plane-key-contract-race");
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(racing_store),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));

    let query_options = QueryOptions {
        filter: Some(ws_path_filter(workspace, path)),
        ..QueryOptions::default()
    };
    let security_ctx = SecurityContext::system();
    let result = read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await;
    let result = match result {
        Ok(result) => result,
        Err(_) => panic!("revision change must fall back to authoritative scan"),
    };

    assert!(
        fired.load(Ordering::SeqCst),
        "forced lookup interleaving did not run"
    );
    assert_eq!(result.entities.len(), 1);
    assert_eq!(
        result.entities[0]["entity_id"].as_str(),
        Some(entity_id),
        "scan result: {}",
        result.entities[0]
    );
    assert!(
        EventStore::key_index_backfilled_types(&inner, tenant.as_str())
            .await
            .expect("read invalidated coverage")
            .is_empty(),
        "contract-B write must invalidate contract-A coverage"
    );
}
