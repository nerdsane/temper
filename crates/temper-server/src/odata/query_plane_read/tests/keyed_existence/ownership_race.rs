//! Forced ownership/body-generation interleavings for exact-key reads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::storage::{BackendLabel, BoxedEventStore};
use temper_runtime::persistence::{
    EntityKeyLookup, EntityKeyRow, EventMetadata, IndexReconciliation, PersistenceAppend,
    PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};

fn field_update_event(persistence_id: &str, path: &str, token: &str) -> PersistenceEnvelope {
    let timestamp = sim_now();
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Temper.Internal.FieldUpdate.v1".to_string(),
        payload: serde_json::json!({
            "schema": "temper.field-update.v1",
            "fields": {"Path": path},
            "replace": false,
            "idempotency_key": token,
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: persistence_id.to_string(),
        },
    }
}

fn key_row(workspace: &str, path: &str) -> EntityKeyRow {
    EntityKeyRow {
        key_name: "ws_path".to_string(),
        key_hash: ws_path_hash(workspace, path),
    }
}

fn snapshot(entity_id: &str, workspace: &str, path: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
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
    .expect("snapshot serialization")
}

async fn seed_owner(
    store: &SimEventStore,
    tenant: &TenantId,
    entity_id: &str,
    workspace: &str,
    path: &str,
) {
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    EventStore::append_with_index_rows(
        store,
        &persistence_id,
        0,
        &[field_update_event(&persistence_id, path, "seed-owner")],
        &[key_row(workspace, path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("seed owner journal and key row");
    EventStore::save_snapshot(
        store,
        &persistence_id,
        1,
        &snapshot(entity_id, workspace, path),
    )
    .await
    .expect("seed owner snapshot");
}

#[tokio::test]
async fn stable_key_row_materializes_its_journal_generation_not_stale_actor() {
    let (_guard, _clock, _ids) = install_deterministic_context(250);
    let tenant = TenantId::default();
    let workspace = "ws-stale-actor";
    let stale_path = "/before";
    let current_path = "/after";
    let entity_id = "ord-stale-resident";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("key-owner-stale-actor");

    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({
                "Id": entity_id,
                "WorkspaceId": workspace,
                "Path": stale_path,
            }),
        )
        .await
        .expect("spawn resident actor at sequence one");
    let sequence = current_sequence(&store, &tenant, "Order", entity_id).await;
    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        sequence,
        &[field_update_event(
            &persistence_id,
            current_path,
            "external-rename",
        )],
        &[key_row(workspace, current_path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("commit rename outside the resident actor");
    EventStore::mark_key_index_backfilled(
        &store,
        tenant.as_str(),
        "Order",
        ORDER_KEY_SET_SIGNATURE,
    )
    .await
    .expect("mark exact coverage");

    let stale = state
        .get_tenant_entity_state(&tenant, "Order", entity_id)
        .await
        .expect("resident actor remains readable");
    assert_eq!(stale.state.fields["Path"], stale_path);

    let options = QueryOptions {
        filter: Some(ws_path_filter(workspace, current_path)),
        ..QueryOptions::default()
    };
    let security_ctx = SecurityContext::system();
    let result = read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await;
    let result = match result {
        Ok(result) => result,
        Err(_) => panic!("stable key row must materialize its journal generation"),
    };

    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], entity_id);
    assert_eq!(result.entities[0]["fields"]["Path"], current_path);
    assert_eq!(result.entities[0]["sequence_nr"], sequence + 1);
}

#[derive(Clone)]
struct AbaTransferStore {
    inner: SimEventStore,
    tenant: String,
    a_id: String,
    b_id: String,
    workspace: String,
    target_path: String,
    a_other_path: String,
    b_other_path: String,
    moved_once: Arc<AtomicBool>,
    lookup_calls: Arc<AtomicUsize>,
}

impl AbaTransferStore {
    fn a_persistence_id(&self) -> String {
        format!("{}:Order:{}", self.tenant, self.a_id)
    }

    fn b_persistence_id(&self) -> String {
        format!("{}:Order:{}", self.tenant, self.b_id)
    }

    async fn transfer(&self, restore_a: bool) -> Result<(), PersistenceError> {
        let (a_path, b_path, expected, token) = if restore_a {
            (&self.target_path, &self.b_other_path, 2, "aba-restore-a")
        } else {
            (&self.a_other_path, &self.target_path, 1, "aba-move-to-b")
        };
        let a_pid = self.a_persistence_id();
        let b_pid = self.b_persistence_id();
        EventStore::append_batch(
            &self.inner,
            &[
                PersistenceAppend {
                    persistence_id: a_pid.clone(),
                    expected_sequence: expected,
                    events: vec![field_update_event(&a_pid, a_path, token)],
                    key_rows: vec![key_row(&self.workspace, a_path)],
                    reconcile_keys: true,
                    key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
                },
                PersistenceAppend {
                    persistence_id: b_pid.clone(),
                    expected_sequence: expected,
                    events: vec![field_update_event(&b_pid, b_path, token)],
                    key_rows: vec![key_row(&self.workspace, b_path)],
                    reconcile_keys: true,
                    key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
                },
            ],
        )
        .await?;
        Ok(())
    }
}

impl EventStore for AbaTransferStore {
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
        if persistence_id == self.a_persistence_id()
            && !self.moved_once.swap(true, Ordering::SeqCst)
        {
            self.transfer(false).await?;
        }
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
        EventStore::lookup_by_key(&self.inner, tenant, entity_type, key_name, key_hash).await
    }

    async fn lookup_by_key_with_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<EntityKeyLookup>, PersistenceError> {
        let call = self.lookup_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 2 {
            self.transfer(true).await?;
        }
        EventStore::lookup_by_key_with_sequence(
            &self.inner,
            tenant,
            entity_type,
            key_name,
            key_hash,
        )
        .await
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

#[tokio::test]
async fn same_owner_aba_retries_the_new_journal_generation() {
    let (_guard, _clock, _ids) = install_deterministic_context(251);
    let tenant = TenantId::default();
    let workspace = "ws-aba";
    let target_path = "/target";
    let a_other_path = "/a-other";
    let b_other_path = "/b-other";
    let a_id = "ord-aba-a";
    let b_id = "ord-aba-b";
    let inner = SimEventStore::no_faults(251);
    seed_owner(&inner, &tenant, a_id, workspace, target_path).await;
    seed_owner(&inner, &tenant, b_id, workspace, b_other_path).await;
    EventStore::mark_key_index_backfilled(
        &inner,
        tenant.as_str(),
        "Order",
        ORDER_KEY_SET_SIGNATURE,
    )
    .await
    .expect("mark exact coverage");

    let lookup_calls = Arc::new(AtomicUsize::new(0));
    let racing = AbaTransferStore {
        inner,
        tenant: tenant.to_string(),
        a_id: a_id.to_string(),
        b_id: b_id.to_string(),
        workspace: workspace.to_string(),
        target_path: target_path.to_string(),
        a_other_path: a_other_path.to_string(),
        b_other_path: b_other_path.to_string(),
        moved_once: Arc::new(AtomicBool::new(false)),
        lookup_calls: lookup_calls.clone(),
    };
    let mut state = build_order_state("key-owner-aba");
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(racing),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));
    let options = QueryOptions {
        filter: Some(ws_path_filter(workspace, target_path)),
        ..QueryOptions::default()
    };
    let security_ctx = SecurityContext::system();
    let result = read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await;
    let result = match result {
        Ok(result) => result,
        Err(_) => panic!("ABA must retry to a stable owner generation"),
    };

    assert_eq!(lookup_calls.load(Ordering::SeqCst), 3);
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], a_id);
    assert_eq!(result.entities[0]["fields"]["Path"], target_path);
    assert_eq!(result.entities[0]["sequence_nr"], 3);
}
