//! A withheld state timer can be recovered by an install without restarting.

use super::*;
use temper_runtime::persistence::{EventStore, PersistenceError, StateTimerRecord};
use temper_server::StorageStack;
use temper_server::storage::StateTimerStore;
use temper_spec::csdl::parse_csdl;

/// Install a changed spec that preserves the timed state's declaration.
fn install(state: &temper_server::ServerState, revision: u64) {
    let job = format!("{}\n# install revision {revision}\n", specs::JOB);
    let mut registry = state.registry.write().unwrap();
    registry.register_tenant(
        TENANT,
        parse_csdl(specs::CSDL).unwrap(),
        specs::CSDL.to_string(),
        &[("Job", job.as_str()), ("Pass", specs::PASS)],
    );
}

/// The rows committed beside the journal, as in the deployed stores.
struct CommittedRows(SimEventStore);

#[async_trait::async_trait]
impl StateTimerStore for CommittedRows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.0.upsert_state_timer(row);
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id);
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

fn run_seed(seed: u64, committed: bool) {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
        people: Mutex::new(["setter".to_string()].into_iter().collect()),
        outsiders: Default::default(),
    };
    let mut process = Process::start(&world, "withheld-install");
    if committed {
        // Switch the empty server to read the same rows its journal commits.
        process.runtime.block_on(async {
            process
                .state
                .set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
            process.state.state_timer_rows = Some(Arc::new(CommittedRows(world.events.clone())));
        });
    }
    let row = || {
        if committed {
            world.events.state_timer("alpha:Job:child")
        } else {
            world.rows.row(TENANT, "child")
        }
    };
    process.runtime.block_on(async {
        let tenant = TenantId::new(TENANT);
        let state = &process.state;
        let by = if seed.is_multiple_of(2) { By::Themselves } else { By::Agent };
        let caller = Setter::Leaves(by).caller("setter");
        let set = state
            .dispatch_tenant_action(&tenant, "Job", "child", "Start", json!({}), &caller)
            .await
            .unwrap();
        assert!(set.success, "seed {seed}: timer must be set");
        let original = row().expect("timer row");
        world.people.lock().unwrap().clear();
        state.authz.reload_tenant_policies(TENANT, &world.cedar()).unwrap();
        pass(WAIT + Duration::from_secs(30)).await;
        assert_eq!(
            state.state_timeout_tracker.pending_snapshot(),
            vec![("Job".to_string(), 0)],
            "seed {seed}: the withheld task must have ended",
        );
        let retained = row().expect("withheld row retained");
        assert_eq!(retained.arm_id, original.arm_id);
        assert_eq!(retained.armed_by, original.armed_by);
        assert_eq!(fire_count(&world).await, 0, "seed {seed}: lost standing withholds the move");

        // An install while access is still lost must ask the saved setter
        // again, rather than turning the row into a service-owned timer.
        install(state, 1);
        state.follow_spec_changes().await;
        settle().await;
        assert!(row().is_some(), "seed {seed}: still withheld");
        assert_eq!(fire_count(&world).await, 0, "seed {seed}: install must respect lost standing");

        world.people.lock().unwrap().insert("setter".to_string());
        state.authz.reload_tenant_policies(TENANT, &world.cedar()).unwrap();
        install(state, 2);
        state.follow_spec_changes().await;
        settle().await;
        // Repeating the walk must not duplicate a fire or resurrect its row.
        install(state, 3);
        state.follow_spec_changes().await;
        pass(HORIZON).await;
        assert_eq!(fire_count(&world).await, 1,
            "seed {seed}: restoring standing and installing must fire exactly once without a restart");
        assert!(row().is_none(), "seed {seed}: fired row is removed");
    });
    process.kill(&world, Duration::ZERO);
}

/// Read the journal directly: a point read could itself rehydrate and rearm.
async fn fire_count(world: &World) -> usize {
    world
        .events
        .read_events("alpha:Job:child", 0)
        .await
        .unwrap()
        .iter()
        .filter(|event| event.event_type == "Expire")
        .count()
}

#[test]
fn dst_withheld_state_timer_recovers_through_install() {
    for seed in 0..SEEDS {
        run_seed(seed, false);
    }
}

#[test]
fn dst_withheld_committed_state_timer_recovers_through_install() {
    for seed in 0..SEEDS {
        run_seed(seed, true);
    }
}
