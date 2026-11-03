//! A committed batch reset survives a stop before post-dispatch timer work.
use super::*;
use crate::storage::StateTimerStore;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context, sim_now};

#[path = "../../../../tests/dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

/// The same sim store supplies both committed rows and boot's timer reads.
struct TimerRows(SimEventStore);

#[async_trait::async_trait]
impl StateTimerStore for TimerRows {
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}

#[test]
fn committed_composite_reset_survives_restart_before_timer_arming() {
    for seed in 0..8 {
        let clock = Arc::new(process_clock::ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let store = SimEventStore::no_faults(seed);
        let first = runtime();
        let reset_at = first.block_on(async {
            clock.begin();
            let (mut state, _) = item_state(Path::Atomic);
            state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
            state.state_timer_rows = Some(Arc::new(TimerRows(store.clone())));
            transition(&state, Path::Direct, "Move", &person("bob")).await;
            let old = store.list_state_timers("default").remove(0);
            tokio::time::sleep(Duration::from_millis(1_500 + seed * 100)).await;
            let reset_at = sim_now();
            // The batch lands, but its lost answer prevents all subsequent
            // invalidation, actor reload and timer arming in this process.
            store.land_next_appends_then_fail("default:Item:item-1", 1);
            let mut writes = vec![json!({"entity_type": "Item", "entity_id": ITEM,
                "action": "Nudge", "params": {}})];
            if seed % 2 == 1 {
                // A later non-resetting event advances the stream and changes
                // a field used by the timer. Preserve the reset's capture.
                writes.push(json!({"entity_type": "Item", "entity_id": ITEM,
                    "action": "Note", "params": {"moved_by": "after-reset"}}));
            }
            let result = state
                .apply_composite_integration_result(
                    &TenantId::default(),
                    "Item",
                    ITEM,
                    "Decide",
                    &json!({"sub_writes": writes}),
                    &person("alice"),
                )
                .await;
            assert!(result.is_err(), "the committed batch must lose its answer");
            let journal = store.dump_journal("default:Item:item-1");
            assert!(journal.iter().any(|event| event.event_type == "Nudge"));
            let row = store.list_state_timers("default").remove(0);
            assert_ne!(
                row.arm_id, old.arm_id,
                "seed {seed}: the commit kept the old timer"
            );
            assert_eq!(row.started_at, reset_at.to_rfc3339());
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&row.params).unwrap()["moved_by"],
                "alice"
            );
            assert_eq!(
                row.sequence_nr,
                journal
                    .iter()
                    .find(|event| event.event_type == "Nudge")
                    .unwrap()
                    .sequence_nr
            );
            assert_eq!(
                crate::request_context::armed_by::decode_armed_by(&row.armed_by)
                    .unwrap()
                    .agent_id
                    .as_deref(),
                Some("alice")
            );
            clock.end(Duration::from_millis(100));
            reset_at
        });
        // Drop the runtime, killing every actor and timer, before booting
        // against the same durable journal and rows.
        drop(first);
        let second = runtime();
        second.block_on(async {
            clock.begin();
            let (mut state, _) = item_state(Path::Atomic);
            state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
            state.state_timer_rows = Some(Arc::new(TimerRows(store.clone())));
            if let Some(boot) = state.start_runtime().await {
                boot.await.unwrap();
            }
            let before_deadline = reset_at + chrono::Duration::milliseconds(2_750);
            tokio::time::sleep((before_deadline - sim_now()).to_std().unwrap()).await;
            assert_eq!(
                item(&state).await.status,
                "Moved",
                "seed {seed}: old deadline fired"
            );
            let after = settle(&state, Duration::from_secs(1)).await;
            assert_eq!(after.status, "Ticked", "seed {seed}: reset never fired");
            assert_eq!(
                after.fields["ticked_by"], "alice",
                "seed {seed}: stale caller"
            );
            assert_eq!(
                after.fields["moved_by"], "alice",
                "seed {seed}: stale arguments"
            );
            assert_eq!(
                store
                    .dump_journal("default:Item:item-1")
                    .iter()
                    .filter(|event| event.event_type == "Tick")
                    .count(),
                1
            );
        });
    }
}
