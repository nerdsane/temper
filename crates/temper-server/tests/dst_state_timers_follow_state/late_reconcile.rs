//! A row read that overlaps an unrecorded reset must not resurrect the old arm.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_server::entity_actor::{EventOrigin, EventVia};
use temper_server::storage::StateTimerStore;
use tokio::sync::Notify;

/// Hold one storage response after it has taken its snapshot.
struct PausedRows {
    inner: SimTimerRows,
    pause: AtomicBool,
    entered: Notify,
    resume: Notify,
}

#[async_trait::async_trait]
impl StateTimerStore for PausedRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.inner.upsert_state_timer(timer).await
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.inner
            .delete_state_timer(tenant, entity_type, entity_id, arm_id)
            .await
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        self.inner.list_state_timers(tenant).await
    }

    async fn state_timer_row(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<StateTimerRecord>, PersistenceError> {
        let row = self
            .inner
            .state_timer_row(tenant, entity_type, entity_id)
            .await?;
        if self.pause.swap(false, Ordering::Relaxed) {
            self.entered.notify_one();
            self.resume.notified().await;
        }
        Ok(row)
    }
}

const POLLER: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Waiting"]
[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"
[[action]]
name = "Poll"
from = ["Waiting"]
[[state_timeout]]
state = "Waiting"
after_seconds = 10
on_timeout = "Poll"
reset_on = ["Poll"]
"#;

#[test]
fn a_late_row_read_cannot_replace_a_newer_unrecorded_reset() {
    for seed in 0..8 {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            clock.begin();
            let mut registry = SpecRegistry::new();
            registry.register_tenant(
                "alpha",
                parse_csdl(CSDL).unwrap(),
                CSDL.into(),
                &[("Job", POLLER)],
            );
            let rows = Arc::new(PausedRows {
                inner: SimTimerRows::new(seed),
                pause: AtomicBool::new(false),
                entered: Notify::new(),
                resume: Notify::new(),
            });
            let mut server = ServerState::from_registry(ActorSystem::new("late-reset"), registry);
            server.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(seed), None));
            server.state_timer_rows = Some(rows.clone());
            server
                .authz
                .reload_tenant_policies("alpha", "permit(principal, action, resource);")
                .unwrap();
            server.start_runtime().await.unwrap().await.unwrap();
            let tenant = TenantId::new("alpha");
            let id = "reset-during-follow";
            server
                .get_or_create_tenant_entity(&tenant, "Job", id, json!({}))
                .await
                .unwrap();
            let caller = AgentContext::default();
            let start = server
                .dispatch_tenant_action(&tenant, "Job", id, "Start", json!({}), &caller)
                .await
                .unwrap();
            assert!(start.success);
            settle().await;
            let next_spec = POLLER.replace("after_seconds = 10", "after_seconds = 11");
            server.registry.write().unwrap().register_tenant(
                "alpha",
                parse_csdl(CSDL).unwrap(),
                CSDL.into(),
                &[("Job", &next_spec)],
            );
            rows.pause.store(true, Ordering::Relaxed);
            let follow = tokio::spawn({
                let server = server.clone();
                async move { server.follow_spec_changes().await }
            });
            rows.entered.notified().await;
            tokio::time::advance(Duration::from_secs(1)).await;
            let machine = AgentContext {
                event_origin: Some(EventOrigin {
                    via: Some(EventVia::Reaction),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let reset = server
                .dispatch_tenant_action(&tenant, "Job", id, "Poll", json!({}), &machine)
                .await
                .unwrap();
            assert!(reset.success && reset.unrecorded_at.is_some());
            assert_eq!(reset.state.sequence_nr, start.state.sequence_nr);
            let fresh = rows.inner.row("alpha", id).unwrap();
            rows.resume.notify_one();
            follow.await.unwrap();
            settle().await;
            assert_eq!(
                rows.inner.row("alpha", id).unwrap(),
                fresh,
                "seed {seed}: reconciliation must keep the newer reset's clock and arm"
            );
        });
    }
}
