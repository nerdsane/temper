use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use temper_runtime::persistence::{
    EntityKeyRow, EntityVectorRow, EventMetadata, EventStore, IndexReconciliation,
    PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_server::key_index::canonical_key_hash;

use super::*;

const VECTORED_ITEM_IOA: &str =
    include_str!("../../../../test-fixtures/specs/vectored_item.ioa.toml");
pub(super) fn vectored_item_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(
        VECTORED_ITEM_IOA,
    )))
}
#[derive(Clone)]
pub(super) struct ConflictBeforeAppendStore {
    pub(super) inner: SimEventStore,
    conflicts: Arc<Mutex<BTreeMap<String, VecDeque<PersistenceEnvelope>>>>,
    ambiguous_commits: Arc<Mutex<BTreeMap<String, usize>>>,
    ambiguous_storage_commits: Arc<Mutex<BTreeMap<String, usize>>>,
}

impl ConflictBeforeAppendStore {
    pub(super) fn new(seed: u64) -> Self {
        Self {
            inner: SimEventStore::no_faults(seed),
            conflicts: Arc::new(Mutex::new(BTreeMap::new())),
            ambiguous_commits: Arc::new(Mutex::new(BTreeMap::new())),
            ambiguous_storage_commits: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub(super) fn queue_conflicts(&self, persistence_id: &str, events: Vec<PersistenceEnvelope>) {
        self.conflicts
            .lock()
            .expect("conflict script lock poisoned")
            .insert(persistence_id.to_string(), events.into());
    }

    fn scripted_conflict(&self, persistence_id: &str) -> Option<PersistenceEnvelope> {
        let mut conflicts = self
            .conflicts
            .lock()
            .expect("conflict script lock poisoned");
        let event = conflicts.get_mut(persistence_id)?.pop_front();
        if conflicts
            .get(persistence_id)
            .is_some_and(VecDeque::is_empty)
        {
            conflicts.remove(persistence_id);
        }
        event
    }

    pub(super) fn commit_then_report_conflict(&self, persistence_id: &str) {
        self.ambiguous_commits
            .lock()
            .expect("ambiguous commit script lock poisoned")
            .insert(persistence_id.to_string(), 1);
    }

    fn take_ambiguous_commit(&self, persistence_id: &str) -> bool {
        let mut commits = self
            .ambiguous_commits
            .lock()
            .expect("ambiguous commit script lock poisoned");
        let Some(remaining) = commits.get_mut(persistence_id) else {
            return false;
        };
        *remaining -= 1;
        if *remaining == 0 {
            commits.remove(persistence_id);
        }
        true
    }

    pub(super) fn commit_then_report_storage_failure(&self, persistence_id: &str) {
        self.ambiguous_storage_commits
            .lock()
            .expect("ambiguous storage commit script lock poisoned")
            .insert(persistence_id.to_string(), 1);
    }

    fn take_ambiguous_storage_commit(&self, persistence_id: &str) -> bool {
        let mut commits = self
            .ambiguous_storage_commits
            .lock()
            .expect("ambiguous storage commit script lock poisoned");
        let Some(remaining) = commits.get_mut(persistence_id) else {
            return false;
        };
        *remaining -= 1;
        if *remaining == 0 {
            commits.remove(persistence_id);
        }
        true
    }
}

impl EventStore for ConflictBeforeAppendStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.inner
            .append(persistence_id, expected_sequence, events)
            .await
    }

    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconciliation: IndexReconciliation,
    ) -> Result<u64, PersistenceError> {
        if self.take_ambiguous_storage_commit(persistence_id) {
            self.inner
                .append_with_index_rows(
                    persistence_id,
                    expected_sequence,
                    events,
                    key_rows,
                    vector_rows,
                    reconciliation,
                )
                .await?;
            return Err(PersistenceError::Storage(
                "scripted post-commit projection metadata failure".to_string(),
            ));
        }
        if self.take_ambiguous_commit(persistence_id) {
            let actual = self
                .inner
                .append_with_index_rows(
                    persistence_id,
                    expected_sequence,
                    events,
                    key_rows,
                    vector_rows,
                    reconciliation,
                )
                .await?;
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual,
            });
        }
        if let Some(conflict) = self.scripted_conflict(persistence_id) {
            let actual = self
                .inner
                .append(persistence_id, expected_sequence, &[conflict])
                .await?;
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual,
            });
        }
        self.inner
            .append_with_index_rows(
                persistence_id,
                expected_sequence,
                events,
                key_rows,
                vector_rows,
                reconciliation,
            )
            .await
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.inner.append_batch(appends).await
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.inner.read_events(persistence_id, from_sequence).await
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.inner
            .save_snapshot(persistence_id, sequence_nr, snapshot)
            .await
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        self.inner.load_snapshot(persistence_id).await
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.inner.list_entity_ids(tenant).await
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.inner
            .list_entity_ids_by_type(tenant, entity_type)
            .await
    }
}

pub(super) fn concurrent_add_item(persistence_id: &str, product_id: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "AddItem".to_string(),
        payload: serde_json::json!({
            "action": "AddItem",
            "from_status": "Draft",
            "to_status": "Draft",
            "timestamp": sim_now(),
            "params": {"ProductId": product_id}
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.to_string(),
        },
    }
}

pub(super) fn order_key_hash(workspace: &str, path: &str) -> String {
    canonical_key_hash(
        "ws_path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        &serde_json::Map::from_iter([
            ("WorkspaceId".to_string(), serde_json::json!(workspace)),
            ("Path".to_string(), serde_json::json!(path)),
        ]),
    )
    .expect("complete order key")
}
pub(super) async fn update_fields(
    actor_ref: &temper_runtime::actor::ActorRef<EntityMsg>,
    fields: serde_json::Value,
    replace: bool,
) -> EntityResponse {
    actor_ref
        .ask(
            EntityMsg::UpdateFields {
                fields,
                replace,
                idempotency_key: Some(format!("field-update:{}", sim_uuid())),
                expected_spec_generation: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("actor should respond")
}
