//! Storage-boundary interleavings; router, authorization and actors stay real.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use temper_runtime::persistence::{
    PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
};
use temper_server::{StorageStack, storage::BoxedEventStore};

struct InterleavedStore {
    inner: SimEventStore,
    deleter: ServerState,
    reads: AtomicUsize,
    delete_after_read: usize,
    fail_read: usize,
    delete_before_append: bool,
    persistent_failure: bool,
}

impl EventStore for InterleavedStore {
    async fn append(
        &self,
        id: &str,
        expected: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        if self.delete_before_append
            && id == "default:Order:target"
            && events.iter().any(|e| e.event_type == "AddItem")
        {
            self.deleter
                .delete_tenant_entity(&TenantId::default(), "Order", "target")
                .await
                .unwrap();
        }
        self.inner.append(id, expected, events).await
    }
    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.inner.append_batch(appends).await
    }
    async fn read_events(
        &self,
        id: &str,
        from: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let n = if id == "default:Order:target" {
            self.reads.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            0
        };
        if self.fail_read != 0
            && (n == self.fail_read || (self.persistent_failure && n > self.fail_read))
        {
            return Err(PersistenceError::Storage(
                "injected admission read failure".into(),
            ));
        }
        let result = self.inner.read_events(id, from).await;
        if n != 0 && n == self.delete_after_read {
            self.deleter
                .delete_tenant_entity(&TenantId::default(), "Order", "target")
                .await
                .unwrap();
        }
        result
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
    async fn load_snapshot(&self, id: &str) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        self.inner.load_snapshot(id).await
    }
    async fn save_snapshot(
        &self,
        id: &str,
        seq: u64,
        bytes: &[u8],
    ) -> Result<(), PersistenceError> {
        self.inner.save_snapshot(id, seq, bytes).await
    }
}

async fn interleaved(
    seed: u64,
    delete_after_read: usize,
    fail_read: usize,
    delete_before_append: bool,
    persistent_failure: bool,
) -> (ServerState, SimEventStore) {
    let store = SimEventStore::no_faults(seed);
    let deleter = fixture(store.clone(), "interleaving-writer");
    assert_eq!(
        post(&deleter, "/tdata/Orders", json!({"Id":"target"}))
            .await
            .0,
        StatusCode::CREATED
    );
    let mut reader = fixture(store.clone(), "interleaving-reader");
    let mut stack = StorageStack::from_sim(store.clone(), None);
    stack.events = BoxedEventStore::from_arc(Arc::new(InterleavedStore {
        inner: store.clone(),
        deleter,
        reads: AtomicUsize::new(0),
        delete_after_read,
        fail_read,
        delete_before_append,
        persistent_failure,
    }));
    reader.set_storage_stack(stack);
    (reader, store)
}

#[tokio::test]
async fn deletion_between_check_and_use_never_bootstraps_or_executes() {
    for seed in [52901, 52902, 52903, 52904, 52905, 52906] {
        let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(seed);
        let delete_after_read = 1 + (seed % 2) as usize;
        let (state, store) = interleaved(seed, delete_after_read, 0, false, false).await;
        let (status, body) = post(
            &state,
            "/tdata/Orders('target')/Temper.AddItem",
            json!({"ProductId":"p", "Quantity":1}),
        )
        .await;
        eprintln!(
            "bound-action race seed={seed} delete_after_read={delete_after_read} status={status}"
        );
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "EntityNotFound");
        assert_eq!(state.active_actor_count(), 0);
        assert!(!state.entity_exists(&TenantId::default(), "Order", "target"));
        let events = store.dump_journal("default:Order:target");
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event_type, "Deleted");
    }
}

#[tokio::test]
async fn unavailable_startup_read_cannot_become_bootstrap() {
    let (state, store) = interleaved(52907, 0, 3, false, true).await;
    let (status, body) = post(
        &state,
        "/tdata/Orders('target')/Temper.AddItem",
        json!({"ProductId":"p", "Quantity":1}),
    )
    .await;
    assert!(status.is_server_error(), "{body}");
    assert_eq!(state.active_actor_count(), 0);
    assert!(!state.entity_exists(&TenantId::default(), "Order", "target"));
    let events = store.dump_journal("default:Order:target");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "Created");
}

#[tokio::test]
async fn deletion_at_append_invalidates_authorization_before_retry() {
    let (state, store) = interleaved(52908, 0, 0, true, false).await;
    let (status, body) = post(
        &state,
        "/tdata/Orders('target')/Temper.AddItem",
        json!({"ProductId":"p", "Quantity":1}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("authorization became stale")
    );
    assert!(!state.entity_exists(&TenantId::default(), "Order", "target"));
    let events = store.dump_journal("default:Order:target");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].event_type, "Deleted");
}

#[tokio::test]
async fn supervised_recovery_after_one_read_failure_still_cannot_append_created() {
    let (state, store) = interleaved(52909, 0, 3, false, false).await;
    let (status, body) = post(
        &state,
        "/tdata/Orders('target')/Temper.AddItem",
        json!({"ProductId":"p", "Quantity":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = store.dump_journal("default:Order:target");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "Created");
    assert_eq!(events[1].event_type, "AddItem");
}
