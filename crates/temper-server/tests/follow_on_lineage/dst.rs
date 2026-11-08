//! Seeded replay coverage for scheduled post-dispatch effects.

use super::*;

#[derive(Default)]
pub(super) struct RingHookCalls(pub(super) std::sync::atomic::AtomicUsize);

impl temper_server::state::custom_effects::CustomEffectHandler for RingHookCalls {
    fn handle(
        &self,
        _effect: &str,
        _entity_type: &str,
        _entity_id: &str,
        _fields: &serde_json::Value,
        _server: &ServerState,
    ) -> Result<(), String> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Exercise child delivery through the real dispatcher and simulated journal.
/// Each seed varies the order of retries and independent scheduled identities.
#[tokio::test(start_paused = true)]
async fn dst_scheduled_replies_run_each_hook_once() {
    use temper_runtime::scheduler::{install_deterministic_context, sim_uuid};
    use temper_server::StorageStack;
    use temper_store_sim::SimEventStore;

    let spec = format!(
        r#"{BELL_SPEC}
[[action.triggers]]
name = "count_ring"
kind = "hook"
hook = "CountRing"
"#
    );
    for seed in 0..32 {
        let (_guard, clock, _ids) = install_deterministic_context(seed);
        let mut state = build_state_for_tenant("temper-system", "http://127.0.0.1:9", &spec);
        state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(seed), None));
        let calls = std::sync::Arc::new(RingHookCalls::default());
        state.custom_effect_handler = Some(calls.clone());
        let mut delivered = std::collections::BTreeSet::new();
        for step in 0..24 {
            let parent = format!("press-{}", sim_uuid().as_u128() % 3);
            let key =
                temper_server::idempotency::derived_idempotency_key(&parent, "scheduled:Ring", "0");
            delivered.insert(key.clone());
            let agent = AgentContext {
                idempotency_key: Some(temper_server::idempotency::IdempotencyKey::FollowUp(key)),
                ..AgentContext::default()
            };
            let response = state
                .dispatch_tenant_action(
                    &TenantId::new("temper-system"),
                    "Bell",
                    "b1",
                    "Ring",
                    serde_json::json!({}),
                    &agent,
                )
                .await
                .expect("scheduled delivery succeeds");
            assert!(
                response.success,
                "seed {seed}, step {step}: {:?}",
                response.error
            );
            assert_eq!(
                calls.0.load(std::sync::atomic::Ordering::SeqCst),
                delivered.len(),
                "seed {seed}, step {step}: repeated scheduled identity repeated its effect"
            );
            clock.advance();
        }
    }
}
