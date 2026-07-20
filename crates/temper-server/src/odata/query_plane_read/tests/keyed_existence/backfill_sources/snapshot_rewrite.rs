use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::storage::{BackendLabel, BoxedEventStore, StorageStack};
use temper_runtime::persistence::{
    KeyIndexBackfillFence, PersistenceAppend, PersistenceAppendResult, PersistenceError,
};

#[derive(Clone)]
struct SnapshotRewriteDuringBackfillStore {
    inner: SimEventStore,
    persistence_id: String,
    replacement: Vec<u8>,
    rewritten: Arc<AtomicBool>,
    rewrite_on_snapshot_load: bool,
    rewrite_on_append: bool,
    append_attempts: Arc<AtomicUsize>,
}

impl EventStore for SnapshotRewriteDuringBackfillStore {
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

    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[EntityKeyRow],
        vector_rows: &[temper_runtime::persistence::EntityVectorRow],
        reconciliation: IndexReconciliation,
    ) -> Result<u64, PersistenceError> {
        if persistence_id == self.persistence_id {
            self.append_attempts.fetch_add(1, Ordering::SeqCst);
            if self.rewrite_on_append && !self.rewritten.swap(true, Ordering::SeqCst) {
                let sequence_nr = EventStore::load_snapshot(&self.inner, persistence_id)
                    .await?
                    .map(|(sequence_nr, _)| sequence_nr)
                    .expect("append rewrite fixture requires a captured snapshot");
                EventStore::save_snapshot(
                    &self.inner,
                    persistence_id,
                    sequence_nr,
                    &self.replacement,
                )
                .await?;
            }
        }
        EventStore::append_with_index_rows(
            &self.inner,
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
        let captured = EventStore::load_snapshot(&self.inner, persistence_id).await?;
        if self.rewrite_on_snapshot_load
            && persistence_id == self.persistence_id
            && !self.rewritten.swap(true, Ordering::SeqCst)
        {
            let sequence_nr = captured
                .as_ref()
                .map(|(sequence_nr, _)| *sequence_nr)
                .expect("rewrite fixture requires a captured snapshot");
            EventStore::save_snapshot(&self.inner, persistence_id, sequence_nr, &self.replacement)
                .await?;
        }
        Ok(captured)
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

    async fn list_entity_ids_for_key_reconciliation(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        EventStore::list_entity_ids_for_key_reconciliation(&self.inner, tenant, entity_type).await
    }

    async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        expected_sequence: u64,
        contract_fence: KeyIndexBackfillFence<'_>,
        key_rows: &[EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        if !self.rewritten.swap(true, Ordering::SeqCst) {
            let sequence_nr = EventStore::load_snapshot(&self.inner, &self.persistence_id)
                .await?
                .map(|(sequence_nr, _)| sequence_nr)
                .expect("rewrite fixture requires a captured snapshot");
            EventStore::save_snapshot(
                &self.inner,
                &self.persistence_id,
                sequence_nr,
                &self.replacement,
            )
            .await?;
        }
        EventStore::backfill_entity_keys(
            &self.inner,
            tenant,
            entity_type,
            entity_id,
            expected_sequence,
            contract_fence,
            key_rows,
        )
        .await
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

    async fn begin_key_index_backfill(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<u64, PersistenceError> {
        EventStore::begin_key_index_backfill(&self.inner, tenant, entity_type, key_set).await
    }

    async fn mark_key_index_backfilled_if_revision(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
        expected_revision: u64,
    ) -> Result<bool, PersistenceError> {
        EventStore::mark_key_index_backfilled_if_revision(
            &self.inner,
            tenant,
            entity_type,
            key_set,
            expected_revision,
        )
        .await
    }
}

#[tokio::test]
async fn same_sequence_snapshot_rewrite_invalidates_the_backfill_row() {
    let (_guard, _clock, _ids) = install_deterministic_context(269);
    let tenant = TenantId::default();
    let before_workspace = "ws-before-rewrite";
    let after_workspace = "ws-after-rewrite";
    let journal_path = "/journal-path";
    let entity_id = "ord-snapshot-rewrite";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let inner = SimEventStore::no_faults(269);

    EventStore::save_snapshot(
        &inner,
        &persistence_id,
        1,
        &legacy_snapshot(entity_id, before_workspace, "/snapshot-path"),
    )
    .await
    .expect("seed snapshot baseline before repair");
    EventStore::append(
        &inner,
        &persistence_id,
        0,
        &[journal_path_delta(
            &persistence_id,
            journal_path,
            "snapshot-rewrite-delta",
        )],
    )
    .await
    .expect("seed journal delta at the same numeric sequence");

    let store = SnapshotRewriteDuringBackfillStore {
        inner: inner.clone(),
        persistence_id: persistence_id.clone(),
        replacement: legacy_snapshot(entity_id, after_workspace, "/snapshot-path"),
        rewritten: Arc::new(AtomicBool::new(false)),
        rewrite_on_snapshot_load: false,
        rewrite_on_append: false,
        append_attempts: Arc::new(AtomicUsize::new(0)),
    };
    let mut state = build_order_state("snapshot-rewrite-backfill");
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));

    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        !state
            .key_index_backfill_complete(&tenant, "Order", ORDER_KEY_SET_SIGNATURE)
            .await,
        "a same-sequence snapshot content change must reject the captured repair row"
    );
    assert_eq!(
        EventStore::lookup_by_key(
            &inner,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash(before_workspace, journal_path),
        )
        .await
        .expect("lookup stale mixed ownership"),
        None,
        "the pre-rewrite snapshot component must not be certified"
    );

    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", ORDER_KEY_SET_SIGNATURE)
            .await,
        "the next pass must converge on the stable replacement snapshot"
    );
    assert_eq!(
        EventStore::lookup_by_key(
            &inner,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash(after_workspace, journal_path),
        )
        .await
        .expect("lookup stable replacement ownership")
        .as_deref(),
        Some(entity_id)
    );
}

#[tokio::test]
async fn actor_recovery_retries_a_same_sequence_snapshot_rewrite_before_upgrade() {
    let (_guard, _clock, _ids) = install_deterministic_context(279);
    let tenant = TenantId::default();
    let entity_id = "ord-actor-snapshot-rewrite";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let before_workspace = "ws-before-actor-rewrite";
    let after_workspace = "ws-after-actor-rewrite";
    let journal_path = "/journal-generation";
    let (state, inner) = build_order_state_with_sim("actor-snapshot-rewrite");
    EventStore::save_snapshot(
        &inner,
        &persistence_id,
        1,
        &legacy_snapshot(entity_id, before_workspace, "/snapshot-generation"),
    )
    .await
    .expect("seed captured actor snapshot");
    EventStore::append(
        &inner,
        &persistence_id,
        0,
        &[journal_path_delta(
            &persistence_id,
            journal_path,
            "actor-snapshot-rewrite",
        )],
    )
    .await
    .expect("seed equal-sequence journal generation");

    let rewritten = Arc::new(AtomicBool::new(false));
    let store = BoxedEventStore::new(SnapshotRewriteDuringBackfillStore {
        inner,
        persistence_id,
        replacement: legacy_snapshot(entity_id, after_workspace, "/snapshot-generation"),
        rewritten: rewritten.clone(),
        rewrite_on_snapshot_load: true,
        rewrite_on_append: false,
        append_attempts: Arc::new(AtomicUsize::new(0)),
    });
    let table = state
        .registry
        .read()
        .expect("registry lock")
        .get_table_live(&tenant, "Order")
        .expect("Order transition table")
        .read()
        .expect("table lock")
        .clone();
    let recovered = crate::entity_actor::recover_entity_state_from_store(
        tenant.as_str(),
        "Order",
        entity_id,
        &table,
        &store,
        BackendLabel::Sim,
        &serde_json::json!({}),
        None,
        false,
    )
    .await
    .expect("recover one closed snapshot/journal generation");

    assert!(rewritten.load(Ordering::SeqCst));
    assert_eq!(
        recovered.fields["WorkspaceId"], after_workspace,
        "recovery must retry the replacement snapshot instead of overwriting it with a stale provenance upgrade"
    );
    assert_eq!(recovered.fields["Path"], journal_path);
}

async fn actor_append_rewrite_fixture(
    seed: u64,
    entity_id: &str,
    system_name: &str,
) -> (
    ServerState,
    SimEventStore,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
) {
    let tenant = TenantId::default();
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let inner = SimEventStore::no_faults(seed);
    EventStore::save_snapshot(
        &inner,
        &persistence_id,
        1,
        &legacy_snapshot(entity_id, "ws-before-append", "/journal-generation"),
    )
    .await
    .expect("seed actor snapshot before append race");
    EventStore::append(
        &inner,
        &persistence_id,
        0,
        &[journal_path_delta(
            &persistence_id,
            "/journal-generation",
            "append-race-journal-generation",
        )],
    )
    .await
    .expect("seed equal-sequence journal generation");

    let rewritten = Arc::new(AtomicBool::new(false));
    let append_attempts = Arc::new(AtomicUsize::new(0));
    let store = SnapshotRewriteDuringBackfillStore {
        inner: inner.clone(),
        persistence_id,
        replacement: legacy_snapshot(entity_id, "ws-after-append", "/journal-generation"),
        rewritten: rewritten.clone(),
        rewrite_on_snapshot_load: false,
        rewrite_on_append: true,
        append_attempts: append_attempts.clone(),
    };
    let mut state = build_order_state(system_name);
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));
    (state, inner, rewritten, append_attempts)
}

#[tokio::test]
async fn field_update_retries_when_snapshot_rewrites_inside_the_append_boundary() {
    let (_guard, _clock, _ids) = install_deterministic_context(280);
    let tenant = TenantId::default();
    let entity_id = "ord-field-update-append-rewrite";
    let (state, inner, rewritten, append_attempts) =
        actor_append_rewrite_fixture(280, entity_id, "field-update-append-rewrite").await;

    let updated = state
        .update_tenant_entity_fields(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"Path": "/after-field-update"}),
            false,
            Some("field-update-append-rewrite".to_string()),
        )
        .await
        .expect("field update must recover and retry the rewritten generation");

    assert!(rewritten.load(Ordering::SeqCst));
    assert_eq!(append_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(updated.state.fields["WorkspaceId"], "ws-after-append");
    assert_eq!(updated.state.fields["Path"], "/after-field-update");
    assert_eq!(
        EventStore::lookup_by_key(
            &inner,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash("ws-after-append", "/after-field-update"),
        )
        .await
        .expect("lookup recovered field-update ownership")
        .as_deref(),
        Some(entity_id)
    );
    assert_eq!(
        EventStore::lookup_by_key(
            &inner,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash("ws-before-append", "/after-field-update"),
        )
        .await
        .expect("lookup stale field-update ownership"),
        None
    );
}

#[tokio::test]
async fn domain_action_retries_when_snapshot_rewrites_inside_the_append_boundary() {
    let (_guard, _clock, _ids) = install_deterministic_context(281);
    let tenant = TenantId::default();
    let entity_id = "ord-domain-action-append-rewrite";
    let (state, inner, rewritten, append_attempts) =
        actor_append_rewrite_fixture(281, entity_id, "domain-action-append-rewrite").await;
    let agent = AgentContext::for_service("arn238-domain-action-append-rewrite");

    let updated = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            entity_id,
            "AddItem",
            serde_json::json!({}),
            &agent,
        )
        .await
        .expect("domain action dispatch must complete after source-fence retry");

    assert!(updated.success, "domain action failed: {:?}", updated.error);
    assert!(rewritten.load(Ordering::SeqCst));
    assert_eq!(append_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(updated.state.fields["WorkspaceId"], "ws-after-append");
    assert_eq!(updated.state.fields["Path"], "/journal-generation");
    assert_eq!(updated.state.item_count, 1);
    assert_eq!(
        EventStore::lookup_by_key(
            &inner,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash("ws-after-append", "/journal-generation"),
        )
        .await
        .expect("lookup recovered domain-action ownership")
        .as_deref(),
        Some(entity_id)
    );
    assert_eq!(
        EventStore::lookup_by_key(
            &inner,
            tenant.as_str(),
            "Order",
            "ws_path",
            &ws_path_hash("ws-before-append", "/journal-generation"),
        )
        .await
        .expect("lookup stale domain-action ownership"),
        None
    );
}
