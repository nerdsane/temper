use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::storage::{BackendLabel, BoxedEventStore, QueryPlaneStore};
use temper_runtime::persistence::{
    EntityKeyLookup, JournalBoundary, PersistenceAppend, PersistenceAppendResult, PersistenceError,
};

mod catalog_deleted;
mod complete_coverage;

pub(super) fn complete_field_update(
    persistence_id: &str,
    entity_id: &str,
    workspace: &str,
    path: &str,
    token: &str,
) -> PersistenceEnvelope {
    let mut event = field_update_event(persistence_id, path, token);
    event.payload["fields"] = serde_json::json!({
        "Id": entity_id,
        "WorkspaceId": workspace,
        "Path": path,
    });
    event
}

pub(super) async fn read_path(
    state: &ServerState,
    tenant: &TenantId,
    workspace: &str,
    path: &str,
) -> Result<QueryPlaneReadResult, QueryPlaneReadError> {
    let options = QueryOptions {
        filter: Some(ws_path_filter(workspace, path)),
        ..QueryOptions::default()
    };
    let security_ctx = SecurityContext::system();
    read_entity_set_page(QueryPlaneReadRequest {
        state,
        tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await
}

fn expect_read(
    result: Result<QueryPlaneReadResult, QueryPlaneReadError>,
    message: &str,
) -> QueryPlaneReadResult {
    match result {
        Ok(result) => result,
        Err(_) => panic!("{message}"),
    }
}

#[tokio::test]
async fn incomplete_scan_prefers_equal_sequence_journal_over_snapshot_source() {
    let (_guard, _clock, _ids) = install_deterministic_context(252);
    let tenant = TenantId::default();
    let workspace = "ws-source-replacement";
    let snapshot_path = "/snapshot-generation";
    let journal_path = "/journal-generation";
    let entity_id = "ord-equal-sequence-source";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (state, store) = build_order_state_with_sim("incomplete-source-replacement");

    EventStore::save_snapshot(
        &store,
        &persistence_id,
        1,
        &snapshot(entity_id, workspace, snapshot_path),
    )
    .await
    .expect("seed snapshot-only generation");
    EventStore::append_with_index_rows(
        &store,
        &persistence_id,
        0,
        &[complete_field_update(
            &persistence_id,
            entity_id,
            workspace,
            journal_path,
            "replace-snapshot-source",
        )],
        &[key_row(workspace, journal_path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("commit equal-sequence journal replacement");

    let old = expect_read(
        read_path(&state, &tenant, workspace, snapshot_path).await,
        "source-replacement old-key read",
    );
    assert!(
        old.entities.is_empty(),
        "a journal generation must replace equal-sequence snapshot ownership"
    );
    let current = expect_read(
        read_path(&state, &tenant, workspace, journal_path).await,
        "source-replacement current-key read",
    );
    assert_eq!(current.entities.len(), 1);
    assert_eq!(current.entities[0]["entity_id"], entity_id);
    assert_eq!(current.entities[0]["fields"]["Path"], journal_path);
    assert_eq!(current.entities[0]["sequence_nr"], 1);
}

#[derive(Clone, Copy)]
enum BoundaryMutationMode {
    ReturnCapturedBoundary,
    ReturnCurrentBoundary,
}

#[derive(Clone)]
struct BoundaryMutationStore {
    inner: SimEventStore,
    persistence_id: String,
    entity_id: String,
    workspace: String,
    path: String,
    expected_sequence: u64,
    trigger_call: usize,
    mode: BoundaryMutationMode,
    boundary_calls: Arc<AtomicUsize>,
}

impl BoundaryMutationStore {
    async fn append_mutation(&self) -> Result<(), PersistenceError> {
        EventStore::append_with_index_rows(
            &self.inner,
            &self.persistence_id,
            self.expected_sequence,
            &[complete_field_update(
                &self.persistence_id,
                &self.entity_id,
                &self.workspace,
                &self.path,
                "boundary-race",
            )],
            &[key_row(&self.workspace, &self.path)],
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
                vectors: false,
            },
        )
        .await?;
        Ok(())
    }
}

impl EventStore for BoundaryMutationStore {
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

    async fn journal_boundary(
        &self,
        persistence_id: &str,
    ) -> Result<JournalBoundary, PersistenceError> {
        let captured = EventStore::journal_boundary(&self.inner, persistence_id).await?;
        let call = self.boundary_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if persistence_id == self.persistence_id && call == self.trigger_call {
            self.append_mutation().await?;
            return match self.mode {
                BoundaryMutationMode::ReturnCapturedBoundary => Ok(captured),
                BoundaryMutationMode::ReturnCurrentBoundary => {
                    EventStore::journal_boundary(&self.inner, persistence_id).await
                }
            };
        }
        Ok(captured)
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
        EventStore::lookup_by_key(&self.inner, tenant, entity_type, key_name, key_hash).await
    }

    async fn lookup_by_key_with_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<EntityKeyLookup>, PersistenceError> {
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

fn install_boundary_store(
    state: &mut ServerState,
    store: BoundaryMutationStore,
    query_plane: Option<Arc<dyn QueryPlaneStore>>,
) {
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store),
        None,
        None,
        None,
        None,
        query_plane,
        None,
        None,
        None,
    ));
}

#[tokio::test]
async fn journal_zero_actor_source_is_closed_by_a_second_boundary_read() {
    let (_guard, _clock, _ids) = install_deterministic_context(253);
    let tenant = TenantId::default();
    let workspace = "ws-zero-source-race";
    let snapshot_path = "/snapshot-only";
    let journal_path = "/first-journal";
    let entity_id = "ord-zero-source-race";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let (mut state, inner) = build_order_state_with_sim("incomplete-zero-source-race");

    EventStore::save_snapshot(
        &inner,
        &persistence_id,
        1,
        &snapshot(entity_id, workspace, snapshot_path),
    )
    .await
    .expect("seed snapshot-only actor source");
    state
        .get_or_create_tenant_entity(&tenant, "Order", entity_id, serde_json::json!({}))
        .await
        .expect("hydrate resident actor from snapshot-only state");
    let boundary_calls = Arc::new(AtomicUsize::new(0));
    install_boundary_store(
        &mut state,
        BoundaryMutationStore {
            inner,
            persistence_id,
            entity_id: entity_id.to_string(),
            workspace: workspace.to_string(),
            path: journal_path.to_string(),
            expected_sequence: 0,
            trigger_call: 1,
            mode: BoundaryMutationMode::ReturnCapturedBoundary,
            boundary_calls: boundary_calls.clone(),
        },
        None,
    );

    let current = expect_read(
        read_path(&state, &tenant, workspace, journal_path).await,
        "journal-zero source transition must stabilize",
    );
    assert_eq!(current.entities.len(), 1);
    assert_eq!(current.entities[0]["entity_id"], entity_id);
    assert_eq!(current.entities[0]["fields"]["Path"], journal_path);
    assert!(
        boundary_calls.load(Ordering::SeqCst) >= 2,
        "the compatibility actor source must be closed by a journal re-read"
    );
}

#[tokio::test]
async fn incomplete_scan_retries_when_journal_advances_after_replay() {
    let (_guard, _clock, _ids) = install_deterministic_context(254);
    let tenant = TenantId::default();
    let workspace = "ws-post-replay-race";
    let initial_path = "/initial";
    let raced_path = "/advanced";
    let entity_id = "ord-post-replay-race";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let inner = SimEventStore::no_faults(254);
    EventStore::append_with_index_rows(
        &inner,
        &persistence_id,
        0,
        &[complete_field_update(
            &persistence_id,
            entity_id,
            workspace,
            initial_path,
            "initial-journal",
        )],
        &[key_row(workspace, initial_path)],
        &[],
        IndexReconciliation {
            keys: true,
            key_set_signature: Some(ORDER_KEY_SET_SIGNATURE.to_string()),
            vectors: false,
        },
    )
    .await
    .expect("seed initial journal generation");
    let boundary_calls = Arc::new(AtomicUsize::new(0));
    let racing = BoundaryMutationStore {
        inner,
        persistence_id,
        entity_id: entity_id.to_string(),
        workspace: workspace.to_string(),
        path: raced_path.to_string(),
        expected_sequence: 1,
        trigger_call: 3,
        mode: BoundaryMutationMode::ReturnCurrentBoundary,
        boundary_calls: boundary_calls.clone(),
    };
    let mut state = build_order_state("incomplete-post-replay-race");
    install_boundary_store(&mut state, racing, None);

    let current = expect_read(
        read_path(&state, &tenant, workspace, raced_path).await,
        "journal advance must retry to a stable generation",
    );
    assert_eq!(current.entities.len(), 1);
    assert_eq!(current.entities[0]["fields"]["Path"], raced_path);
    assert_eq!(current.entities[0]["sequence_nr"], 2);
    assert_eq!(boundary_calls.load(Ordering::SeqCst), 5);
}
