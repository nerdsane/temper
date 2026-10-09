//! Production-boundary review regressions; the original lifecycle fixture is shared.
use super::tests::fixture::*;
use crate::request_context::AgentContext;
use crate::state::DispatchCommand;
use serde_json::json;
use std::time::Duration;
use temper_runtime::{scheduler::install_deterministic_context, tenant::TenantId};
use temper_store_sim::SimEventStore;

async fn capped_node(target: &str, seed: u64) {
    let (_guard, _, _) = install_deterministic_context(seed);
    let spec = format!(
        r#"
[automaton]
name="Node"
states=["Idle","Running"]
initial="Idle"
[admission]
max_concurrent_actions={{Start=1}}
queue_timeout_seconds=1
[[action]]
name="Start"
from=["Idle"]
to="Running"
[[action.triggers]]
name="child"
kind="entity"
target_entity="Node"
target_action="Start"
resolve_target={{kind="static",entity_id="{target}"}}
"#
    );
    let mut state = state_with_specs(&[("Node", &spec)]);
    let store = SimEventStore::no_faults(seed);
    state.set_storage_stack(crate::StorageStack::from_sim(store.clone(), None));
    let context = AgentContext {
        idempotency_key: Some("K".into()),
        ..AgentContext::system()
    };
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        state.dispatch_tenant_action_with_completion(
            DispatchCommand {
                tenant: &TenantId::default(),
                entity_type: "Node",
                entity_id: "a",
                action: "Start",
                params: json!({}),
                agent_ctx: &context,
                await_integration: false,
                await_reactions: true,
            },
            None,
            Some(0),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(result.response.success);
    assert!(!result.reactions.is_empty());
    assert!(
        result.reactions.iter().all(|reaction| reaction.success),
        "capped descendant was deferred: {:?}",
        result.reactions
    );
    assert_eq!(action_count_id(&store, "Node", "a", "Start"), 1);
    assert_eq!(action_count_id(&store, "Node", target, "Start"), 1);
}

#[tokio::test(start_paused = true)]
async fn actor_admission_ends_before_same_action_cross_entity_reactions() {
    capped_node("b", 51990).await;
}

#[tokio::test(start_paused = true)]
async fn capped_reaction_back_edge_reaches_receipt_cycle_handling() {
    capped_node("a", 51991).await;
}
