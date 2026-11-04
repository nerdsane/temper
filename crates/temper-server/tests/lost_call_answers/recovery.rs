//! Restart and delayed boot recovery at the real-engine boundary.

use super::*;

#[test]
fn a_refused_answer_with_failed_cleanup_is_not_lost_at_restart() {
    for seed in [10, 11, 12] {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let store = SimEventStore::no_faults(seed);
        let first = Process::start(
            &store,
            &clock,
            "cleanup-a",
            &json!({ "params": { "score": 10 } }),
        );
        first.runtime.block_on(async {
            store.fail_next_scheduled_deletes(1);
            ask(&first.state).await;
            assert!(
                run_until(|| store.scheduled_delete_failures_pending() == 0).await,
                "the refused answer reached cleanup and deletion failed"
            );
            assert_eq!(
                store.list_scheduled_actions(TENANT).await.unwrap().len(),
                1,
                "the test must stop with cleanup still pending"
            );
        });
        first.kill(&clock, Duration::from_secs(10));
        assert_eq!(
            status_after_restart(&store, &clock),
            "Asked",
            "seed {seed}: recovery must not turn an ended call into a lost one"
        );
        assert!(
            !first_journal(&store)
                .iter()
                .any(|action| action == "GiveUp")
        );
        assert!(
            block(store.list_scheduled_actions(TENANT))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn delayed_boot_recovery_does_not_answer_a_current_process_call() {
    for seed in [13, 14, 15] {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let store = SimEventStore::no_faults(seed);
        store.fail_next_scheduled_lists(TENANT, 1);
        let process = Process::start(
            &store,
            &clock,
            "boot-retry",
            &json!({ "params": { "score": 70 } }),
        );
        process.runtime.block_on(async {
            process
                .state
                .get_or_create_tenant_entity(&TenantId::new(TENANT), "Lookup", ID, json!({}))
                .await
                .unwrap();
            store.inject_append_delay(JOURNAL, Duration::ZERO);
            store.inject_append_delay(JOURNAL, Duration::from_secs(3600));
            ask(&process.state).await;
            assert!(
                run_until(|| !store.append_faults_pending(JOURNAL)).await,
                "the live call's answer is waiting on storage"
            );
            // Boot's retry now reads the current call's deadline, while its
            // successful answer still waits on the actor's storage write.
            tokio::time::advance(Duration::from_secs(2)).await;
            for _ in 0..600 {
                tokio::task::yield_now().await;
            }
            assert!(
                process.state.armed_scheduled_actions().is_empty(),
                "seed {seed}: boot recovery must not arm a live call's deadline"
            );
            tokio::time::advance(Duration::from_secs(4 * DEADLINE_SECS)).await;
            for _ in 0..600 {
                tokio::task::yield_now().await;
            }
            assert!(
                !journal(&store)
                    .await
                    .iter()
                    .any(|action| action == "GiveUp")
            );
            // The write outlasted its handler's limit, so the record gave it up.
            // The answer is kept and sent again, each wait on the clock's next
            // step: its second write lands.
            for _ in 0..8 {
                tokio::time::advance(Duration::from_secs(60)).await;
                for _ in 0..600 {
                    tokio::task::yield_now().await;
                }
            }
            assert_eq!(
                journal(&store).await.last().map(String::as_str),
                Some("Finish")
            );
        });
        process.kill(&clock, Duration::ZERO);
    }
}

#[test]
fn failed_cleanup_does_not_block_later_independent_calls() {
    let source = LOOKUP.replace(
        "[[action]]\nname = \"Finish\"",
        r#"[[action.triggers]]
name = "later"
kind = "wasm"
module = "later"
on_success = "Finish"
on_failure = "GiveUp"

[action.triggers.config]
timeout_secs = "30"

[[action]]
name = "Finish""#,
    );
    assert_ne!(source, LOOKUP, "the fixture has two independent triggers");
    for seed in [16, 17, 18] {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let store = SimEventStore::no_faults(seed);
        let process = Process::start_with_modules(
            &store,
            &clock,
            "independent-calls",
            &source,
            &[
                ("lookup", json!({ "params": { "score": 10 } })),
                ("later", json!({ "params": { "score": 70 } })),
            ],
        );
        process.runtime.block_on(async {
            store.fail_next_scheduled_deletes(1);
            ask(&process.state).await;
            assert!(
                run_until(|| store
                    .dump_journal(JOURNAL)
                    .iter()
                    .any(|event| event.event_type == "Finish"))
                .await,
                "seed {seed}: the second call must answer while the first call's cleanup waits"
            );
            assert_eq!(store.scheduled_delete_failures_pending(), 0);
            let rows = store.list_scheduled_actions(TENANT).await.unwrap();
            assert_eq!(rows.len(), 1, "only the first call's cleanup is pending");
            assert!(
                rows[0].action.is_empty(),
                "the first call is durably cancelled"
            );
            assert!(rows[0].arm_id.ends_with(".call:lookup"));
            tokio::time::advance(Duration::from_secs(1)).await;
            for _ in 0..600 {
                tokio::task::yield_now().await;
            }
            assert!(
                store
                    .list_scheduled_actions(TENANT)
                    .await
                    .unwrap()
                    .is_empty()
            );
        });
        process.kill(&clock, Duration::ZERO);
    }
}
