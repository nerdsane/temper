//! Seeded, paused-clock regressions using the production timer pipeline.
use super::*;
use std::future::Future;
use std::sync::Arc;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_server::entity_actor::{EventOrigin, EventVia};
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;

/// Read the same rows that the simulated journal commits beside its events.
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
        kind: &str,
        id: &str,
        arm: &str,
    ) -> Result<(), PersistenceError> {
        self.0.delete_state_timer(tenant, kind, id, arm);
        Ok(())
    }
    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

fn simulate<F, Fut>(spec: &str, scenario: F)
where
    F: Fn(ServerState, SimEventStore) -> Fut,
    Fut: Future<Output = ()>,
{
    for seed in 0..8 {
        let clock = Arc::new(super::process_clock::ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            clock.begin();
            let events = SimEventStore::no_faults(seed);
            let mut state = server_with_spec("fire-now-dst", spec, events.clone());
            state.state_timer_rows = Some(Arc::new(CommittedRows(events.clone())));
            state.start_runtime().await.unwrap().await.unwrap();
            scenario(state, events).await;
            clock.end(Duration::ZERO);
        });
    }
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

async fn pass(duration: Duration) {
    settle().await;
    tokio::time::advance(duration).await;
    settle().await;
}

#[test]
fn withheld_acceleration_keeps_its_row_after_the_original_deadline() {
    simulate(CHANGE_IOA, |state, events| async move {
        records(&state, "c1").await;
        act(&state, person("ann"), "c1", "Watch").await;
        let original = events.state_timer("default:Change:c1").expect("armed row");
        state.authz.reload_tenant_policies("default", &format!(
            "{POLICY}\nforbid(principal, action == Action::\"read\", resource) when {{ principal.id == \"ann\" }};"
        )).unwrap();
        pass(Duration::from_secs(1)).await;
        act(&state, hook(), "c1", "Heard").await;
        settle().await;
        assert_eq!(
            read(&state, "Change", "c1", "looked_by").await.0,
            "Watching"
        );
        pass(Duration::from_secs(86401)).await;
        let retained = events
            .state_timer("default:Change:c1")
            .expect("the stale deadline task must preserve the withheld row");
        assert_eq!(retained.arm_id, original.arm_id);
        assert_eq!(retained.armed_by, original.armed_by);
        assert_eq!(
            state.state_timeout_tracker.pending_snapshot(),
            vec![("Change".to_string(), 0)]
        );

        // Restore standing and recover the retained arm through a spec walk.
        state
            .authz
            .reload_tenant_policies("default", POLICY)
            .unwrap();
        let revised = format!("{CHANGE_IOA}\n# recovery install\n");
        state.registry.write().unwrap().register_tenant(
            "default",
            parse_csdl(CSDL).unwrap(),
            CSDL.to_string(),
            &[("Change", revised.as_str()), ("Run", RUN_IOA)],
        );
        state.follow_spec_changes().await;
        settle().await;
        assert_eq!(
            read(&state, "Change", "c1", "looked_by").await,
            ("Looked".into(), json!("ann"))
        );
        assert!(events.state_timer("default:Change:c1").is_none());
    });
}

/// A process whose timer-row writes never became durable, while its live arm survives.
#[derive(Default)]
struct MissingRows {
    writes: std::sync::Mutex<Vec<StateTimerRecord>>,
    // Hold the accelerated task's row write until another delivery commits.
    acceleration_gate: Option<Arc<tokio::sync::Semaphore>>,
}
#[async_trait::async_trait]
impl StateTimerStore for MissingRows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        let accelerated = {
            let mut writes = self.writes.lock().unwrap();
            writes.push(row.clone());
            writes.len() > 1
        };
        if accelerated && let Some(gate) = &self.acceleration_gate {
            gate.acquire().await.unwrap().forget();
        }
        Ok(())
    }
    async fn delete_state_timer(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), PersistenceError> {
        Ok(())
    }
    async fn list_state_timers(&self, _: &str) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(vec![])
    }
}

#[test]
fn rowless_acceleration_keeps_the_live_arms_arguments_and_identity() {
    rowless_deliveries(false);
}

#[test]
fn queued_rowless_deliveries_keep_the_same_arm_when_its_journal_moves_on() {
    rowless_deliveries(true);
}

fn rowless_deliveries(queued: bool) {
    let spec = CHANGE_IOA
        .replace("[[state]]\nname = \"looked_by\"", "[[state]]\nname = \"payload\"\ntype = \"string\"\ninitial = \"\"\n\n[[state]]\nname = \"looked_by\"")
        .replace("name = \"Watch\"\n", "name = \"Watch\"\nparams = [{ name = \"payload\", type = \"string\" }]\n")
        .replace("name = \"Heard\"\n", "name = \"Heard\"\nparams = [{ name = \"payload\", type = \"string\" }]\n")
        .replace("on_timeout = \"Look\"", "on_timeout = \"Look\"\nargs = { captured = \"payload\" }")
        .replace("params = [{ name = \"looked_by\", type = \"string\", source = \"authenticated_subject\" }]", "params = [{ name = \"looked_by\", type = \"string\", source = \"authenticated_subject\" }, { name = \"captured\", type = \"string\" }]");
    simulate(&spec, move |mut state, _| async move {
        let gate = queued.then(|| Arc::new(tokio::sync::Semaphore::new(0)));
        let attempted = Arc::new(MissingRows {
            acceleration_gate: gate.clone(),
            ..Default::default()
        });
        state.state_timer_rows = Some(attempted.clone());
        records(&state, "c1").await;
        let (status, body) = post(
            &state,
            person("ann"),
            "/tdata/Changes('c1')/Temper.Line.Watch",
            json!({"payload":"original"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
        pass(Duration::from_secs(1)).await;
        let delivery = || {
            post(
                &state,
                hook(),
                "/tdata/Changes('c1')/Temper.Line.Heard",
                json!({"payload":"changed"}),
            )
        };
        if queued {
            let delivered = || async {
                let answer = delivery().await;
                gate.as_ref().unwrap().add_permits(1);
                answer
            };
            let (first, second) = tokio::join!(delivered(), delivered());
            assert_eq!(first.0, StatusCode::OK, "{first:?}");
            assert_eq!(second.0, StatusCode::OK, "{second:?}");
        } else {
            let (status, body) = delivery().await;
            assert_eq!(status, StatusCode::OK, "{body:?}");
        }
        settle().await;
        assert_eq!(
            read(&state, "Change", "c1", "captured").await,
            ("Looked".into(), json!("original"))
        );
        assert_eq!(
            read(&state, "Change", "c1", "looked_by").await.1,
            json!("ann")
        );
        let writes = attempted.writes.lock().unwrap();
        assert!(
            writes.len() >= 2,
            "one arm, then acceleration and any journal follow"
        );
        for replacement in &writes[1..] {
            assert_eq!(replacement.arm_id, writes[0].arm_id);
            assert_eq!(replacement.started_at, writes[0].started_at);
            assert_eq!(replacement.params, writes[0].params);
            assert_eq!(replacement.armed_by, writes[0].armed_by);
            assert!(replacement.sequence_nr > writes[0].sequence_nr);
        }
    });
}

#[test]
fn unchanged_machine_delivery_is_recorded_and_brings_the_timer_forward() {
    simulate(CHANGE_IOA, |state, _| async move {
        records(&state, "c1").await;
        act(&state, person("ann"), "c1", "Watch").await;
        pass(Duration::from_secs(1)).await;
        let caller = AgentContext {
            security_ctx: Some(hook()),
            event_origin: Some(EventOrigin {
                via: Some(EventVia::Reaction),
                ..Default::default()
            }),
            ..Default::default()
        };
        let answer = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Change",
                "c1",
                "Heard",
                json!({}),
                &caller,
            )
            .await
            .unwrap();
        assert!(answer.success, "{answer:?}");
        assert!(
            answer.unrecorded_at.is_none(),
            "bringing the timer forward is a recorded effect"
        );
        settle().await;
        assert_eq!(
            read(&state, "Change", "c1", "looked_by").await,
            ("Looked".into(), json!("ann"))
        );
    });
}

#[test]
fn a_delivery_fires_a_repeating_timer_once_then_it_waits_its_budget() {
    let spec = CHANGE_IOA
        .replace("to = \"Looked\"", "")
        .replace("reset_on = [\"Nudge\"]", "reset_on = [\"Nudge\", \"Look\"]");
    simulate(&spec, |state, events| async move {
        records(&state, "c1").await;
        act(&state, person("ann"), "c1", "Watch").await;
        pass(Duration::from_secs(1)).await;
        act(&state, hook(), "c1", "Heard").await;
        settle().await;
        let row = events.state_timer("default:Change:c1").unwrap();
        // Reconcile at the very same clock tick as Heard and the repeating fire.
        let current = state
            .get_tenant_entity_state(&TenantId::default(), "Change", "c1")
            .await
            .unwrap();
        assert_eq!(current.state.status, "Watching");
        let count = |state: &temper_server::entity_actor::EntityState| {
            state.events.iter().filter(|e| e.action == "Look").count()
        };
        assert_eq!(count(&current.state), 1);
        // An install reconciles at the same tick without another delivery.
        let revised = format!(
            "{}\n# repeat reconcile\n",
            CHANGE_IOA
                .replace("to = \"Looked\"", "")
                .replace("reset_on = [\"Nudge\"]", "reset_on = [\"Nudge\", \"Look\"]")
        );
        state.registry.write().unwrap().register_tenant(
            "default",
            parse_csdl(CSDL).unwrap(),
            CSDL.to_string(),
            &[("Change", revised.as_str()), ("Run", RUN_IOA)],
        );
        state.follow_spec_changes().await;
        settle().await;
        pass(Duration::from_secs(86399)).await;
        let current = state
            .get_tenant_entity_state(&TenantId::default(), "Change", "c1")
            .await
            .unwrap();
        assert_eq!(
            count(&current.state),
            1,
            "an older delivery must not re-fire the new arm"
        );
        assert_eq!(
            events.state_timer("default:Change:c1").unwrap().arm_id,
            row.arm_id
        );
        pass(Duration::from_secs(1)).await;
        let current = state
            .get_tenant_entity_state(&TenantId::default(), "Change", "c1")
            .await
            .unwrap();
        assert_eq!(current.state.status, "Watching");
        let next = events.state_timer("default:Change:c1").unwrap();
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&next.started_at).unwrap(),
            chrono::DateTime::parse_from_rfc3339(&row.started_at).unwrap()
                + chrono::Duration::seconds(86400),
            "the next fire waits the full budget"
        );
        assert_ne!(next.arm_id, row.arm_id);
    });
}

#[test]
fn deliveries_are_local_and_do_not_arm_or_reuse_an_older_delivery() {
    let spec = CHANGE_IOA.replace(
        "name = \"Heard\"\nkind = \"input\"\nfrom = [\"Watching\"]",
        "name = \"Heard\"\nkind = \"input\"\nfrom = [\"Waiting\", \"Watching\"]",
    );
    simulate(&spec, |state, events| async move {
        for id in ["c1", "c2", "c3"] {
            records(&state, id).await;
        }
        act(&state, person("ann"), "c1", "Watch").await;
        act(&state, person("bea"), "c2", "Watch").await;
        let other = events.state_timer("default:Change:c2").unwrap();
        pass(Duration::from_secs(1)).await;
        act(&state, hook(), "c3", "Heard").await;
        assert!(
            events.state_timer("default:Change:c3").is_none(),
            "no timer in Waiting"
        );
        act(&state, hook(), "c1", "Heard").await;
        settle().await;
        assert_eq!(read(&state, "Change", "c1", "looked_by").await.0, "Looked");
        assert_eq!(
            read(&state, "Change", "c2", "looked_by").await.0,
            "Watching"
        );
        assert_eq!(
            events.state_timer("default:Change:c2").unwrap().started_at,
            other.started_at
        );
        assert_eq!(
            events.state_timer("default:Change:c2").unwrap().arm_id,
            other.arm_id
        );
        pass(Duration::from_secs(1)).await;
        act(&state, person("ann"), "c3", "Watch").await;
        pass(Duration::from_secs(86399)).await;
        assert_eq!(
            read(&state, "Change", "c3", "looked_by").await.0,
            "Watching",
            "an older Heard cannot shorten a later arm"
        );
        pass(Duration::from_secs(1)).await;
        assert_eq!(read(&state, "Change", "c3", "looked_by").await.0, "Looked");
    });
}
