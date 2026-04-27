//! Integration tests for the full agent chain.
//!
//! Tests the complete flow: StartProcess → PrepareContext → ContextReady →
//! invoke_llm → InferenceCompleteEndTurn → Agent back to Ready.

use std::sync::Arc;

use dd_testcontainers::postgres::PostgresContainer;
use temper_actor_runtime::spec_actor::SpecActorState;
use temper_actor_runtime::{ActorSystem, SchedulerConfig, SpecMessage};
use temper_agents::{
    AGENT_ACTOR_TYPES, MockLlmIntegration, MockToolExecutor, register_agent_actors,
};
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;

static POSTGRES: OnceCell<PostgresContainer> = OnceCell::const_new();

async fn test_pool() -> deadpool_postgres::Pool {
    let pg = POSTGRES
        .get_or_init(|| async {
            let container = PostgresContainer::builder()
                .db("odp_temper")
                .start_async()
                .await
                .expect("failed to start postgres");
            let connstr = format!(
                "host={} port={} user={} password={} dbname={}",
                container.host(),
                container.port(),
                container.user(),
                container.password(),
                container.db()
            );
            let (client, conn) = tokio_postgres::connect(&connstr, NoTls)
                .await
                .expect("connect failed");
            tokio::spawn(conn);
            temper_actor_runtime::schema::create_tables(&client)
                .await
                .expect("schema failed");
            container
        })
        .await;

    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(pg.host().to_string());
    cfg.port = Some(pg.port());
    cfg.user = Some(pg.user().to_string());
    cfg.password = Some(pg.password().to_string());
    cfg.dbname = Some(pg.db().to_string());
    cfg.create_pool(Some(deadpool_postgres::Runtime::Tokio1), NoTls)
        .expect("pool failed")
}

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
    let pool = test_pool().await;
    let system = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    register_agent_actors(&system).await.unwrap();
    system.register(Arc::new(MockLlmIntegration)).await.unwrap();
    system.register(Arc::new(MockToolExecutor)).await.unwrap();

    let namespace = format!("test/session/{}", uuid::Uuid::new_v4());

    // Spawn agent and send Initialize.
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
async fn test_simple_inference_chain() {
    let pool = test_pool().await;
    let system = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    register_agent_actors(&system).await.unwrap();
    system.register(Arc::new(MockLlmIntegration)).await.unwrap();
    system.register(Arc::new(MockToolExecutor)).await.unwrap();

    let namespace = format!("test/session/{}", uuid::Uuid::new_v4());

    // Spawn all session actors in the namespace.
    for actor_type in AGENT_ACTOR_TYPES {
        system.spawn(&namespace, actor_type).await.unwrap();
    }

    // Initialize the agent.
    let agent = temper_actor_runtime::ActorHandle::new(namespace.clone(), "Process");
    system
        .tell(None, &agent, SpecMessage::new("Initialize"))
        .await
        .unwrap();
    run_until_quiescent(&system, 10).await;

    let state = load_actor_state(&pool, &namespace, "Process").await;
    assert_eq!(state.status, "Ready");

    // Send StartProcess.
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

    // Run until chain completes.
    // Flow: Agent → PrepareContext → ContextManager → assemble_context →
    //       MockContextAssembler → ContextReady → ContextManager → Agent →
    //       invoke_llm → MockLlm → InferenceCompleteEndTurn → Agent → Ready
    run_until_quiescent(&system, 50).await;

    let agent_state = load_actor_state(&pool, &namespace, "Process").await;
    assert_eq!(
        agent_state.status, "Ready",
        "Agent should return to Ready after simple inference. Got: {}",
        agent_state.status
    );
    assert_eq!(agent_state.counters.get("turns"), Some(&1usize));
}

#[tokio::test]
async fn test_context_manager_transitions() {
    let pool = test_pool().await;
    let system = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    register_agent_actors(&system).await.unwrap();
    system.register(Arc::new(MockLlmIntegration)).await.unwrap();
    system.register(Arc::new(MockToolExecutor)).await.unwrap();

    let namespace = format!("test/session/{}", uuid::Uuid::new_v4());

    // Spawn ContextManager and its integration.
    system.spawn(&namespace, "ContextManager").await.unwrap();
    system
        .spawn(&namespace, "ContextAssemblerIntegration")
        .await
        .unwrap();
    // Also spawn Agent so ContextReady has somewhere to route.
    system.spawn(&namespace, "Process").await.unwrap();

    let ctx_mgr = temper_actor_runtime::ActorHandle::new(namespace.clone(), "ContextManager");
    system
        .tell(None, &ctx_mgr, SpecMessage::new("PrepareContext"))
        .await
        .unwrap();

    run_until_quiescent(&system, 20).await;

    // ContextManager should have gone Idle → Assembling → Idle.
    let state = load_actor_state(&pool, &namespace, "ContextManager").await;
    assert_eq!(state.status, "Idle");
    assert_eq!(state.counters.get("preparations_done"), Some(&1usize));
}
