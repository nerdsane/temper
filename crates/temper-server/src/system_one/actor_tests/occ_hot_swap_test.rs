//! Spec swaps at the storage conflict boundary exercise the real actor retry.

use std::sync::atomic::AtomicBool;

use temper_runtime::persistence::{
    EntityKeyRow, EntityVectorRow, EventStore, PersistenceAppend, PersistenceAppendResult,
    PersistenceEnvelope, PersistenceError,
};

use super::*;

#[derive(Clone, Copy, Debug)]
enum SpecChange {
    Unchanged,
    Wording,
    RemoveGuard,
}

struct ConflictBoundaryStore {
    sim: SimEventStore,
    table: Arc<RwLock<TransitionTable>>,
    change: SpecChange,
    seed: u64,
    fired: AtomicBool,
}

impl ConflictBoundaryStore {
    fn inject_at_domain_append(&self, id: &str, events: &[PersistenceEnvelope]) {
        if id != format!("{TENANT}:Case:{ENTITY}")
            || !events.iter().any(|event| event.event_type == "Escalate")
            || self.fired.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let spec = match self.change {
            SpecChange::Unchanged => SPEC.to_owned(),
            SpecChange::Wording => SPEC.replace(
                "Has a human been requested?",
                "Is a human explicitly requested?",
            ),
            SpecChange::RemoveGuard => SPEC
                .lines()
                .filter(|line| !line.starts_with("guard = "))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        *self.table.write().unwrap() = TransitionTable::from_ioa_source(&spec);
        // Use the actual simulated store's one-shot conflict. It leaves the
        // journal unchanged and reports actual == expected, isolating spec
        // freshness from entity-state freshness in the production retry path.
        self.sim.inject_concurrency_violations(id, 1);
    }
}

impl EventStore for ConflictBoundaryStore {
    async fn append(
        &self,
        id: &str,
        expected: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_with_index_rows(id, expected, events, &[], &[], false)
            .await
    }

    async fn append_with_index_rows(
        &self,
        id: &str,
        expected: u64,
        events: &[PersistenceEnvelope],
        keys: &[EntityKeyRow],
        vectors: &[EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        for _ in 0..self.seed % 4 {
            tokio::task::yield_now().await;
        }
        self.inject_at_domain_append(id, events);
        self.sim
            .append_with_index_rows(id, expected, events, keys, vectors, reconcile_vectors)
            .await
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.sim.append_batch(appends).await
    }

    async fn read_events(
        &self,
        id: &str,
        from: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.sim.read_events(id, from).await
    }

    async fn save_snapshot(
        &self,
        id: &str,
        sequence: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.sim.save_snapshot(id, sequence, snapshot).await
    }

    async fn load_snapshot(&self, id: &str) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        self.sim.load_snapshot(id).await
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.sim.list_entity_ids(tenant).await
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.sim.list_entity_ids_by_type(tenant, entity_type).await
    }
}

fn faulted_server(
    sim: SimEventStore,
    provider: Arc<TestProvider>,
    change: SpecChange,
    seed: u64,
) -> (ServerState, Arc<ConflictBoundaryStore>) {
    let tenant = TenantId::new(TENANT);
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TENANT,
        temper_spec::csdl::parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Case", SPEC)],
    );
    let store = Arc::new(ConflictBoundaryStore {
        sim: sim.clone(),
        table: registry.get_table_live(&tenant, "Case").unwrap(),
        change,
        seed,
        fired: AtomicBool::new(false),
    });
    let mut stack = StorageStack::from_sim(sim, None);
    stack.events = BoxedEventStore::from_arc(store.clone());
    let mut state =
        ServerState::from_registry(ActorSystem::new("system-one-occ-hot-swap"), registry)
            .with_system_one_provider(provider);
    state.set_storage_stack(stack);
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .unwrap();
    state
        .get_or_spawn_tenant_actor_with_fields(
            &tenant,
            "Case",
            ENTITY,
            json!({"Messages":"Please connect me with a human.","revision":0}),
        )
        .unwrap();
    (state, store)
}

#[tokio::test(flavor = "current_thread")]
async fn spec_changes_during_spurious_conflicts_never_reuse_old_evidence() {
    for seed in 0..32 {
        for change in [
            SpecChange::Unchanged,
            SpecChange::Wording,
            SpecChange::RemoveGuard,
        ] {
            let _context = install_deterministic_context(1_400 + seed);
            let sim = SimEventStore::no_faults(1_400 + seed);
            let provider = TestProvider::new(seed, Answer::Positive);
            let (state, store) = faulted_server(sim.clone(), provider.clone(), change, seed);
            let before = read(&state, TENANT).await.state;
            let result = dispatch(&state, TENANT, "Escalate", json!({}), "old-attempt").await;
            assert!(
                store.fired.load(Ordering::SeqCst),
                "seed={seed} change={change:?}: fault not reached"
            );
            assert_eq!(
                provider.calls(),
                1,
                "seed={seed} change={change:?}: evaluation repeated"
            );
            let current = read(&state, TENANT).await.state;
            let journal = sim.dump_journal(&format!("{TENANT}:Case:{ENTITY}"));
            match change {
                SpecChange::Unchanged => {
                    assert!(
                        result.unwrap().success,
                        "seed={seed}: unchanged-spec conflict refused"
                    );
                    assert_eq!(current.status, "Escalated");
                    assert_eq!(current.sequence_nr, before.sequence_nr + 1);
                    assert_eq!(
                        journal
                            .iter()
                            .filter(|event| event.event_type == "Escalate")
                            .count(),
                        1
                    );
                    assert!(
                        dispatch(&state, TENANT, "Escalate", json!({}), "old-attempt")
                            .await
                            .unwrap()
                            .success
                    );
                }
                _ => {
                    assert!(
                        rejected(&result),
                        "seed={seed} change={change:?}: stale spec evidence committed after conflict replay"
                    );
                    assert_eq!(current.status, "Open");
                    assert_eq!(current.sequence_nr, before.sequence_nr);
                    assert_eq!(current.fields, before.fields);
                    assert_eq!(current.counters, before.counters);
                    assert!(
                        journal.iter().all(|event| event.event_type != "Escalate"),
                        "seed={seed} change={change:?}: stale action entered journal"
                    );
                    assert!(
                        rejected(
                            &dispatch(&state, TENANT, "Escalate", json!({}), "old-attempt").await
                        ),
                        "seed={seed} change={change:?}: old attempt accepted on retry"
                    );
                }
            }
            assert_eq!(
                provider.calls(),
                1,
                "seed={seed} change={change:?}: retry resampled"
            );
        }
    }
}
