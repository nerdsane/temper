//! Durable retry coverage for post-dispatch platform effects.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;
use temper_server::state::custom_effects::CustomEffectHandler;
use temper_server::{ServerState, state::custom_effects};
use temper_store_sim::SimEventStore;

const EFFECT_SPEC: &str = r#"
[automaton]
name = "EffectOwner"
states = ["Ready"]
initial = "Ready"

[[action]]
name = "Publish"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["Value"]
effect = "trigger DurablePublish"
"#;

struct FailOnceEffect {
    attempts: AtomicUsize,
}

#[async_trait::async_trait]
impl CustomEffectHandler for FailOnceEffect {
    async fn handle(
        &self,
        effect_name: &str,
        _entity_type: &str,
        _entity_id: &str,
        _entity_fields: &serde_json::Value,
        _server: &ServerState,
    ) -> Result<(), String> {
        assert_eq!(effect_name, "DurablePublish");
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt == 1 {
            Err("injected publication failure".to_string())
        } else {
            Ok(())
        }
    }
}

fn state_with_handler(
    store: SimEventStore,
    system_name: &str,
    handler: Arc<FailOnceEffect>,
) -> ServerState {
    let mut state = common::build_single_tenant_state_with_store(
        store,
        system_name,
        "default",
        &[("EffectOwner", EFFECT_SPEC)],
    );
    let handler: Arc<dyn custom_effects::CustomEffectHandler> = handler;
    state.custom_effect_handler = Some(handler);
    state
}

#[tokio::test]
async fn failed_custom_effect_replays_exactly_after_process_restart() {
    let (_guard, _clock, _ids) = install_deterministic_context(303);
    let store = SimEventStore::no_faults(303);
    let handler = Arc::new(FailOnceEffect {
        attempts: AtomicUsize::new(0),
    });
    let tenant = TenantId::default();
    let mut agent = AgentContext::for_service("effect-retry-test");
    agent.idempotency_key = Some("publish-once".to_string());

    let first_state = state_with_handler(store.clone(), "effect-first", Arc::clone(&handler));
    let first = first_state
        .dispatch_tenant_action(
            &tenant,
            "EffectOwner",
            "owner-1",
            "Publish",
            serde_json::json!({"Value": "v1"}),
            &agent,
        )
        .await
        .expect("durable action returns its effect outcome");
    assert!(
        !first.success,
        "a committed action must remain retryable while its durable custom effect failed"
    );
    let committed_sequence = first.state.sequence_nr;
    assert!(committed_sequence > 0);
    assert_eq!(handler.attempts.load(Ordering::SeqCst), 1);
    drop(first_state);

    let restarted = state_with_handler(store, "effect-restart", Arc::clone(&handler));
    let retried = restarted
        .dispatch_tenant_action(
            &tenant,
            "EffectOwner",
            "owner-1",
            "Publish",
            serde_json::json!({"Value": "v1"}),
            &agent,
        )
        .await
        .expect("retry recovers the committed transition");

    assert!(retried.success, "the recovered effect retry must complete");
    assert_eq!(
        retried.state.sequence_nr, committed_sequence,
        "effect retry must not append the domain action again"
    );
    assert_eq!(
        handler.attempts.load(Ordering::SeqCst),
        2,
        "the durable idempotency record must replay the original effect after restart"
    );
}
