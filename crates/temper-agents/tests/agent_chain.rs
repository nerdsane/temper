//! Integration tests for the full agent chain.
//!
//! Tests the complete flow: StartProcess → PrepareContext → ContextReady →
//! invoke_llm → InferenceCompleteEndTurn → Agent back to Ready.

use std::sync::Arc;

use temper_actor_runtime::spec_actor::SpecActorState;
use temper_actor_runtime::test_utils::setup_test_pg;
use temper_actor_runtime::{ActorSystem, SchedulerConfig, SpecMessage};
use temper_agents::{MockLlmIntegration, MockToolExecutor, register_agent_actors};

async fn load_actor_state(
    pool: &deadpool_postgres::Pool,
    namespace: &str,
    actor_type: &str,
) -> SpecActorState {
    let client = pool.get().await.unwrap();
    let rows = client
        .query(
            "SELECT state FROM odp_temper.actor_instances WHERE namespace = $1 AND actor_type = $2",
            &[&namespace, &actor_type],
        )
        .await
        .unwrap();
    serde_json::from_slice(&rows[0].get::<_, Vec<u8>>("state")).unwrap()
}

async fn run_until_quiescent(system: &ActorSystem, max_polls: usize) {
    for _ in 0..max_polls {
        let n = system.poll_once().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if n == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let n2 = system.poll_once().await.unwrap();
            if n2 == 0 {
                break;
            }
        }
    }
}

#[tokio::test]
async fn test_agent_initialize() {
    let (pool, _postgres) = setup_test_pg().await;
    let system = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    register_agent_actors(&system).await.unwrap();
    system.register(Arc::new(MockLlmIntegration)).await.unwrap();
    system.register(Arc::new(MockToolExecutor)).await.unwrap();

    let namespace = format!("test/session/{}", uuid::Uuid::new_v4());

    let agent = system.spawn(&namespace, "Process").await.unwrap();
    system
        .tell(None, &agent, SpecMessage::new("Initialize"))
        .await
        .unwrap();

    run_until_quiescent(&system, 10).await;

    let state = load_actor_state(&pool, &namespace, "Process").await;
    assert_eq!(state.status, "Ready");
}

#[tokio::test]
async fn test_inference_chain_discovers_unspawned_registered_siblings() {
    let (pool, _postgres) = setup_test_pg().await;
    let system = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    register_agent_actors(&system).await.unwrap();
    system.register(Arc::new(MockLlmIntegration)).await.unwrap();
    system.register(Arc::new(MockToolExecutor)).await.unwrap();

    let namespace = format!("test/session/{}", uuid::Uuid::new_v4());

    system.spawn(&namespace, "Process").await.unwrap();

    let agent = temper_actor_runtime::ActorHandle::new(namespace.clone(), "Process");
    system
        .tell(None, &agent, SpecMessage::new("Initialize"))
        .await
        .unwrap();
    run_until_quiescent(&system, 10).await;

    let state = load_actor_state(&pool, &namespace, "Process").await;
    assert_eq!(state.status, "Ready");

    system
        .tell(
            None,
            &agent,
            temper_actor_runtime::spec_actor::SpecMessage::with_params(
                "StartProcess",
                serde_json::json!({ "user_prompt": "test question" }),
            ),
        )
        .await
        .unwrap();

    let ((), ()) = tokio::join!(
        run_until_quiescent(&system, 50),
        run_until_quiescent(&system, 50)
    );

    let agent_state = load_actor_state(&pool, &namespace, "Process").await;
    assert_eq!(
        agent_state.status, "Ready",
        "Agent should return to Ready after simple inference. Got: {}",
        agent_state.status
    );
    assert_eq!(agent_state.counters.get("turns"), Some(&1usize));
    let context = load_actor_state(&pool, &namespace, "ContextManager").await;
    assert_eq!(context.counters.get("preparations_done"), Some(&1usize));
    // A new scheduler must not process already-consumed messages again.
    let restarted = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    register_agent_actors(&restarted).await.unwrap();
    restarted
        .register(Arc::new(MockLlmIntegration))
        .await
        .unwrap();
    restarted
        .register(Arc::new(MockToolExecutor))
        .await
        .unwrap();
    let ((), ()) = tokio::join!(
        run_until_quiescent(&system, 5),
        run_until_quiescent(&restarted, 5)
    );
    let after = load_actor_state(&pool, &namespace, "Process").await;
    assert_eq!(after.counters.get("turns"), Some(&1usize));
    assert_eq!(after.fields, agent_state.fields);
}

#[tokio::test]
async fn test_context_manager_transitions() {
    let (pool, _postgres) = setup_test_pg().await;
    let system = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    register_agent_actors(&system).await.unwrap();
    system.register(Arc::new(MockLlmIntegration)).await.unwrap();
    system.register(Arc::new(MockToolExecutor)).await.unwrap();

    let namespace = format!("test/session/{}", uuid::Uuid::new_v4());

    system.spawn(&namespace, "ContextManager").await.unwrap();
    system
        .spawn(&namespace, "ContextAssemblerIntegration")
        .await
        .unwrap();
    system.spawn(&namespace, "Process").await.unwrap();

    let ctx_mgr = temper_actor_runtime::ActorHandle::new(namespace.clone(), "ContextManager");
    system
        .tell(None, &ctx_mgr, SpecMessage::new("PrepareContext"))
        .await
        .unwrap();

    run_until_quiescent(&system, 20).await;

    let state = load_actor_state(&pool, &namespace, "ContextManager").await;
    assert_eq!(state.status, "Idle");
    assert_eq!(state.counters.get("preparations_done"), Some(&1usize));
}
