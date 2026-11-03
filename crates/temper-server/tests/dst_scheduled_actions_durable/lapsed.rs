//! A check scheduled again with every report keeps one row, and a check that
//! comes due after its run is done does not fire.
//!
//! `Launch` and each `Progress` schedule `Check` at the same deadline, as a
//! KatagamiAppRun's Launched and Progressed do. Before arming was idempotent
//! every report added a row, and all of them fired at the deadline; after the
//! run was done each was refused and recorded (on 2 Nov 2026, 20 at once for
//! one run). One run here finishes before the deadline, another is still
//! running at it, so its check shows what a fire records.

use std::sync::Mutex;

use tracing_subscriber::layer::{Context, SubscriberExt};

use super::*;

/// The deadline of every check, from launch.
const CHECK_IN: i64 = 370;

/// The records each trajectory entry was written for, read from the
/// trajectory's `app usage` line (`record_dispatch_trajectory`).
#[derive(Clone, Default)]
struct Trajectory(Arc<Mutex<Vec<(String, String)>>>);

impl Trajectory {
    fn entries(&self, id: &str, action: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(entity_id, entry)| entity_id == id && entry == action)
            .count()
    }
}

#[derive(Default)]
struct Line {
    message: String,
    entity_id: String,
    action: String,
}

impl tracing::field::Visit for Line {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record_debug(field, &value);
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}").trim_matches('"').to_string();
        match field.name() {
            "message" => self.message = text,
            "entity_id" => self.entity_id = text,
            "action" => self.action = text,
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Trajectory {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut line = Line::default();
        event.record(&mut line);
        if line.message.starts_with("app usage:") {
            self.0.lock().unwrap().push((line.entity_id, line.action));
        }
    }
}

/// The record's waiting scheduled actions of `action`.
async fn rows(world: &World, id: &str, action: &str) -> usize {
    world
        .events
        .list_scheduled_actions(TENANT)
        .await
        .unwrap()
        .iter()
        .filter(|row| row.entity_id == id && row.action == action)
        .count()
}

#[test]
fn dst_a_check_scheduled_with_every_report_keeps_one_row_and_lapses_once_done() {
    for seed in 0..8 {
        let trajectory = Trajectory::default();
        let _log = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(trajectory.clone()),
        );
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = World {
            events: SimEventStore::no_faults(seed),
            rows: Arc::new(SimTimerRows::new(seed)),
            clock,
        };
        let mut rng = DeterministicRng::new(seed ^ 0x0c4e_c4ed);
        let process = Process::start(&world, "lapsed");
        process.runtime.block_on(async {
            let state = &process.state;
            pass(Duration::from_millis(rng.next_bound(5_000) as u64)).await;
            let check_at = sim_now() + chrono::Duration::seconds(CHECK_IN);
            let params = json!({ "check_at": check_at.to_rfc3339() });
            for id in ["done", "running"] {
                state
                    .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", id, json!({}))
                    .await
                    .unwrap();
                assert!(act(state, id, "Launch", params.clone()).await);
            }
            for report in 1..=3 {
                pass(Duration::from_secs(1 + rng.next_bound(60) as u64)).await;
                for id in ["done", "running"] {
                    assert!(
                        act(state, id, "Progress", params.clone()).await,
                        "seed {seed}: {id}'s report {report}"
                    );
                }
            }
            settle().await;
            for id in ["done", "running"] {
                assert_eq!(
                    rows(&world, id, "Check").await,
                    1,
                    "seed {seed}: {id}: a check armed for the same deadline four times keeps one row"
                );
            }
            assert!(act(state, "done", "Finish", json!({})).await);

            until(check_at + chrono::Duration::seconds(5)).await;
            settle().await;

            assert!(
                fired_at(&world.events, "done", "Check").await.is_empty(),
                "seed {seed}: a check that came due after its run was done does not fire"
            );
            assert_eq!(
                trajectory.entries("done", "Check"),
                0,
                "seed {seed}: and writes no trajectory entry, no refusal"
            );
            assert_eq!(
                rows(&world, "done", "Check").await,
                0,
                "seed {seed}: its row is removed, so no boot fires it again"
            );
            assert_eq!(
                fired_at(&world.events, "running", "Check").await.len(),
                1,
                "seed {seed}: the running run's one row fires once"
            );
            assert_eq!(
                trajectory.entries("running", "Check"),
                1,
                "seed {seed}: and its fire is recorded once"
            );
            assert_eq!(rows(&world, "running", "Check").await, 0);
        });
        process.kill(&world, Duration::ZERO);
    }
}
