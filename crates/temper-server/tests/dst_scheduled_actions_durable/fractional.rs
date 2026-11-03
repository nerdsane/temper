//! Fractional absolute deadlines, measured from the committed fire history.
use super::*;
use chrono::Timelike;
use std::sync::Mutex;
use temper_server::wake::{SendError, WakeReport, WakeSink};

#[derive(Default)]
struct Reports(Mutex<Vec<WakeReport>>);

#[async_trait::async_trait]
impl WakeSink for Reports {
    async fn send(&self, report: &WakeReport, _: Option<&str>) -> Result<(), SendError> {
        self.0.lock().unwrap().push(report.clone());
        Ok(())
    }
}

#[test]
fn dst_schedule_at_fires_at_its_millisecond_deadline() {
    for seed in 0..16 {
        for delay_ms in [1_798_980, 1_001, 1_999, -999] {
            let clock = Arc::new(ProcessClock::new());
            let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
            let world = World {
                events: SimEventStore::no_faults(seed),
                rows: Arc::new(SimTimerRows::new(seed)),
                clock,
            };
            let process = Process::start(&world, "fractional");
            process.runtime.block_on(async {
                // Resolve from a fractional current time too.
                pass(Duration::from_millis(137 + seed)).await;
                process
                    .state
                    .get_or_create_tenant_entity(
                        &TenantId::new(TENANT),
                        "Job",
                        "fractional",
                        json!({}),
                    )
                    .await
                    .unwrap();
                let now = sim_now();
                let base = if delay_ms == 1_001 || delay_ms == 1_999 {
                    now.with_nanosecond(0).unwrap()
                } else {
                    now
                };
                let target = base + chrono::Duration::milliseconds(delay_ms);
                assert!(
                    act(
                        &process.state,
                        "fractional",
                        "StartAt",
                        json!({"due": target.to_rfc3339()})
                    )
                    .await
                );
                until(target.max(now) + chrono::Duration::milliseconds(5)).await;
                settle().await;
                let fires = fired_at(&world.events, "fractional", "Fire").await;
                assert_eq!(fires.len(), 1, "seed {seed}, delay {delay_ms} ms");
                let lateness = fires[0] - target.max(now);
                assert!(
                    lateness >= chrono::Duration::zero()
                        && lateness <= chrono::Duration::milliseconds(5),
                    "seed {seed}, delay {delay_ms} ms: target {target}, fired {}, lateness {} ms",
                    fires[0],
                    lateness.num_milliseconds()
                );
            });
            process.kill(&world, Duration::ZERO);
        }
    }
}

#[test]
fn dst_fractional_due_time_survives_snapshot_and_journal_recovery_and_is_reported() {
    for seed in 0..16 {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = World {
            events: SimEventStore::no_faults(seed),
            rows: Arc::new(SimTimerRows::new(seed)),
            clock,
        };
        let first = Process::start(&world, "fractional-before");
        let target = first.runtime.block_on(async {
            pass(Duration::from_millis(137)).await;
            let target = sim_now() + chrono::Duration::milliseconds(1_798_980);
            first
                .state
                .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", "restored", json!({}))
                .await
                .unwrap();
            assert!(
                act(
                    &first.state,
                    "restored",
                    "StartAt",
                    json!({"due": target.to_rfc3339()})
                )
                .await
            );
            if seed % 2 == 0 {
                let state = first
                    .state
                    .get_tenant_entity_state(&TenantId::new(TENANT), "Job", "restored")
                    .await
                    .unwrap()
                    .state;
                world
                    .events
                    .save_snapshot(
                        &format!("{TENANT}:Job:restored"),
                        state.sequence_nr,
                        &serde_json::to_vec(&state).unwrap(),
                    )
                    .await
                    .unwrap();
            }
            let rows = world.events.list_scheduled_actions(TENANT).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                DateTime::parse_from_rfc3339(&rows[0].due_at).unwrap(),
                target
            );
            target
        });
        first.kill(&world, Duration::from_millis(12_345 + seed));
        let second = Process::start(&world, "fractional-after");
        second.runtime.block_on(async {
            let reports = Arc::new(Reports::default());
            second.state.set_wake_sink(reports.clone());
            pass(Duration::from_secs(1)).await;
            assert_eq!(
                reports.0.lock().unwrap().last().unwrap().next_due_at,
                Some(target)
            );
            until(target + chrono::Duration::milliseconds(5)).await;
            settle().await;
            let fires = fired_at(&world.events, "restored", "Fire").await;
            assert_eq!(fires.len(), 1);
            assert!(
                fires[0] >= target && fires[0] <= target + chrono::Duration::milliseconds(5),
                "seed {seed}: restored target {target}, fires {fires:?}"
            );
            assert!(
                world
                    .events
                    .list_scheduled_actions(TENANT)
                    .await
                    .unwrap()
                    .is_empty()
            );
        });
        second.kill(&world, Duration::ZERO);
    }
}

#[test]
fn dst_schedule_at_recomputes_its_fractional_wait_after_resume() {
    for seed in 0..16 {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = World {
            events: SimEventStore::no_faults(seed),
            rows: Arc::new(SimTimerRows::new(seed)),
            clock,
        };
        let process = Process::start(&world, "fractional-resume");
        process.runtime.block_on(async {
            process
                .state
                .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", "resumed", json!({}))
                .await
                .unwrap();
            let target = sim_now() + chrono::Duration::milliseconds(1_798_980);
            assert!(
                act(
                    &process.state,
                    "resumed",
                    "StartAt",
                    json!({"due": target.to_rfc3339()})
                )
                .await
            );
            pass(Duration::from_secs(10)).await;
            // The wall clock advances while Tokio's monotonic clock stands still.
            // Leave 1.6 s for the watch to notice the resume before the deadline.
            world.clock.end(Duration::from_millis(1_787_375 + seed));
            world.clock.begin();
            until(target + chrono::Duration::milliseconds(5)).await;
            settle().await;
            let fires = fired_at(&world.events, "resumed", "Fire").await;
            assert_eq!(
                fires.len(),
                1,
                "seed {seed}: target {target}, fires {fires:?}"
            );
            assert!(
                fires[0] >= target && fires[0] <= target + chrono::Duration::milliseconds(5),
                "seed {seed}: resumed target {target}, fires {fires:?}"
            );
        });
        process.kill(&world, Duration::ZERO);
    }
}
