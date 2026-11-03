//! A scheduled child's identity survives its timer's recovery at boot.

use super::*;
use std::sync::Arc;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_server::StorageStack;
use temper_server::idempotency::{IdempotencyKey, derived_idempotency_key};
use temper_store_sim::SimEventStore;

#[test]
fn dst_recovered_schedule_keeps_its_derived_key_and_runs_its_hook_once() {
    let spec = format!(
        r#"{BELL_SPEC}
[[action.triggers]]
name = "count_ring"
kind = "hook"
hook = "CountRing"
"#
    );
    for seed in 0..16 {
        let clock = Arc::new(process_clock::ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let store = SimEventStore::no_faults(seed);
        let calls = Arc::new(dst::RingHookCalls::default());
        let runtime = || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap()
        };
        let build = || {
            let mut state = build_state_for_tenant("temper-system", "http://127.0.0.1:9", &spec);
            state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
            state.custom_effect_handler = Some(calls.clone());
            state
        };
        let parent_key = format!("press-{seed}");
        let first = runtime();
        first.block_on(async {
            clock.begin();
            let state = build();
            press(
                &state,
                &TenantId::new("temper-system"),
                &AgentContext {
                    callback_hops: MAX_CALLBACK_HOPS - 1,
                    idempotency_key: Some(IdempotencyKey::Request(parent_key.clone())),
                    ..AgentContext::default()
                },
            )
            .await;
            assert_eq!(
                store
                    .list_scheduled_actions("temper-system")
                    .await
                    .unwrap()
                    .len(),
                1,
                "seed {seed}: the ring must still be waiting at the stop"
            );
            clock.end(Duration::from_secs(seed % 3));
        });
        drop(first);

        let second = runtime();
        second.block_on(async {
            clock.begin();
            let state = build();
            state.start_runtime().await.unwrap().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            let key = derived_idempotency_key(&parent_key, "scheduled:Ring", "0");
            let events = store.read_events("temper-system:Bell:b1", 0).await.unwrap();
            let rings: Vec<_> = events
                .iter()
                .filter(|event| event.event_type == "Ring")
                .collect();
            assert_eq!(rings.len(), 1, "seed {seed}: recovery must fire once");
            assert_eq!(
                rings[0].payload["idempotency_key"].as_str(),
                Some(key.as_str()),
                "seed {seed}: recovery must keep the child's derived key"
            );
            let reply = state
                .dispatch_tenant_action(
                    &TenantId::new("temper-system"),
                    "Bell",
                    "b1",
                    "Ring",
                    serde_json::json!({}),
                    &AgentContext {
                        idempotency_key: Some(IdempotencyKey::FollowUp(key)),
                        ..AgentContext::default()
                    },
                )
                .await
                .unwrap();
            assert!(reply.success, "seed {seed}: {:?}", reply.error);
            assert_eq!(
                calls.0.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "seed {seed}: redelivery must not repeat the hook"
            );
            assert!(
                store
                    .list_scheduled_actions("temper-system")
                    .await
                    .unwrap()
                    .is_empty()
            );
            clock.end(Duration::ZERO);
        });
    }
}
