//! PostgreSQL regressions for timer snapshots and bounded reaction cascades.

use std::sync::Arc;

use temper_actor_runtime::{ActorSystem, SchedulerConfig, SpecDrivenActor, SpecMessage, schema};
use temper_runtime::reaction::{
    MAX_REACTION_DEPTH, ReactionRegistry, ReactionRule, ReactionTarget, ReactionTrigger,
    TargetResolver,
};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use uuid::Uuid;

static LOCAL_SCHEMA: OnceCell<()> = OnceCell::const_new();

const TIMER_SPEC: &str = r#"
[automaton]
name = "TimerOwner"
states = ["Idle", "Waiting", "Done"]
initial = "Idle"

[[action]]
name = "Schedule"
kind = "input"
from = ["Idle"]
to = "Waiting"
effect = [{ type = "schedule", action = "Wake", delay_seconds = 0 }]

[[action]]
name = "Update"
kind = "input"
from = ["Waiting"]
to = "Waiting"

[[action]]
name = "Wake"
kind = "input"
from = ["Waiting"]
to = "Done"
"#;

const CYCLE_A_SPEC: &str = r#"
[automaton]
name = "CycleA"
states = ["Loop"]
initial = "Loop"

[[state]]
name = "visits"
type = "counter"
initial = "0"

[[action]]
name = "Begin"
kind = "input"
from = ["Loop"]
to = "Loop"
effect = [{ type = "increment", var = "visits" }, { type = "emit", event = "ToB" }]

[[action]]
name = "FromB"
kind = "input"
from = ["Loop"]
to = "Loop"
effect = [{ type = "increment", var = "visits" }, { type = "emit", event = "ToB" }]
"#;

const CYCLE_B_SPEC: &str = r#"
[automaton]
name = "CycleB"
states = ["Loop"]
initial = "Loop"

[[state]]
name = "visits"
type = "counter"
initial = "0"

[[action]]
name = "FromA"
kind = "input"
from = ["Loop"]
to = "Loop"
effect = [{ type = "increment", var = "visits" }, { type = "emit", event = "ToA" }]
"#;

async fn postgres_pool() -> (
    Option<testcontainers::ContainerAsync<Postgres>>,
    deadpool_postgres::Pool,
) {
    let (container, connection) = match std::env::var("TEMPER_TEST_DATABASE_URL") {
        Ok(connection) => (None, connection),
        Err(_) => {
            let container = Postgres::default()
                .start()
                .await
                .expect("PostgreSQL testcontainer must start");
            let host = container.get_host().await.expect("container host");
            let port = container
                .get_host_port_ipv4(5432)
                .await
                .expect("container port");
            let connection =
                format!("host={host} port={port} user=postgres password=postgres dbname=postgres");
            (Some(container), connection)
        }
    };
    let (mut client, driver) = tokio_postgres::connect(&connection, NoTls)
        .await
        .expect("direct PostgreSQL connection");
    tokio::spawn(driver);
    if container.is_none() {
        LOCAL_SCHEMA
            .get_or_init(|| async {
                schema::create_tables(&mut client)
                    .await
                    .expect("actor schema must initialize");
            })
            .await;
    } else {
        schema::create_tables(&mut client)
            .await
            .expect("actor schema must initialize");
    }

    let mut config = deadpool_postgres::Config::new();
    config.url = Some(connection);
    let pool = config
        .create_pool(Some(deadpool_postgres::Runtime::Tokio1), NoTls)
        .expect("actor pool");
    (container, pool)
}

fn reaction(source: &str, event: &str, target: &str, action: &str) -> ReactionRule {
    ReactionRule {
        name: format!("{source}-{event}-{target}"),
        when: ReactionTrigger {
            entity_type: source.into(),
            action: Some(event.into()),
            to_state: None,
        },
        then: ReactionTarget {
            entity_type: target.into(),
            action: action.into(),
        },
        resolve_target: TargetResolver::SameId,
    }
}

async fn register_cycle_actors(system: &ActorSystem) {
    system
        .register(Arc::new(
            SpecDrivenActor::from_ioa(
                CYCLE_A_SPEC,
                ReactionRegistry::from(vec![reaction("CycleA", "ToB", "CycleB", "FromA")]),
            )
            .expect("CycleA spec"),
        ))
        .await
        .expect("register CycleA");
    system
        .register(Arc::new(
            SpecDrivenActor::from_ioa(
                CYCLE_B_SPEC,
                ReactionRegistry::from(vec![reaction("CycleB", "ToA", "CycleA", "FromB")]),
            )
            .expect("CycleB spec"),
        ))
        .await
        .expect("register CycleB");
}

async fn promote_until_namespace_drained(client: &deadpool_postgres::Client, namespace: &str) {
    for _ in 0..1_000 {
        let remaining: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM odp_temper.actor_scheduled_messages WHERE namespace = $1",
                &[&namespace],
            )
            .await
            .expect("count namespace timers")
            .get(0);
        if remaining == 0 {
            return;
        }
        assert_eq!(
            client
                .execute(schema::PROMOTE_DUE_MESSAGES, &[&1_i64])
                .await
                .expect("promote due timer"),
            1,
            "due timers must make bounded promotion progress"
        );
    }
    panic!("timer promotion budget exhausted before namespace drained");
}

#[tokio::test]
async fn scheduled_action_does_not_restore_stale_fields_after_restart() {
    let (_container, pool) = postgres_pool().await;
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    system
        .register(Arc::new(
            SpecDrivenActor::from_ioa(TIMER_SPEC, ReactionRegistry::new()).expect("timer spec"),
        ))
        .await
        .expect("register timer actor");
    let namespace = format!("default/{}", Uuid::new_v4());
    let timer = system
        .spawn_with_fields(
            &namespace,
            "TimerOwner",
            serde_json::json!({"owner": "old"}),
        )
        .await
        .expect("spawn timer owner");

    system
        .tell(None, &timer, SpecMessage::new("Schedule"))
        .await
        .expect("schedule message");
    assert!(system.activate_now(&timer).await.expect("schedule action"));
    system
        .tell(
            None,
            &timer,
            SpecMessage::with_params("Update", serde_json::json!({"owner": "new"})),
        )
        .await
        .expect("update message");
    assert!(system.activate_now(&timer).await.expect("update action"));

    let restarted = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    restarted
        .register(Arc::new(
            SpecDrivenActor::from_ioa(TIMER_SPEC, ReactionRegistry::new()).expect("timer spec"),
        ))
        .await
        .expect("register timer actor after restart");
    let client = pool.get().await.expect("timer promotion client");
    promote_until_namespace_drained(&client, &namespace).await;
    assert!(restarted.activate_now(&timer).await.expect("wake action"));

    let state = restarted
        .get_spec_actor_state(&timer)
        .await
        .expect("timer state");
    assert_eq!(state.status, "Done");
    assert_eq!(state.fields["owner"], "new");
}

#[tokio::test]
async fn reaction_cycle_stops_at_durable_depth_budget_across_restart() {
    let (_container, pool) = postgres_pool().await;
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    register_cycle_actors(&system).await;
    let namespace = format!("default/{}", Uuid::new_v4());
    let cycle_a = system
        .spawn_with_fields(&namespace, "CycleA", serde_json::json!({}))
        .await
        .expect("spawn CycleA");
    let cycle_b = system
        .spawn_with_fields(&namespace, "CycleB", serde_json::json!({}))
        .await
        .expect("spawn CycleB");
    system
        .tell(None, &cycle_a, SpecMessage::new("Begin"))
        .await
        .expect("begin cycle");

    assert!(system.activate_now(&cycle_a).await.expect("cycle depth 0"));
    assert!(system.activate_now(&cycle_b).await.expect("cycle depth 1"));
    assert!(system.activate_now(&cycle_a).await.expect("cycle depth 2"));
    assert!(system.activate_now(&cycle_b).await.expect("cycle depth 3"));

    let restarted = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    register_cycle_actors(&restarted).await;
    let mut activations = 4_u32;
    for _ in 0..MAX_REACTION_DEPTH * 2 {
        let activated_a = restarted
            .activate_now(&cycle_a)
            .await
            .expect("activate CycleA");
        let activated_b = restarted
            .activate_now(&cycle_b)
            .await
            .expect("activate CycleB");
        activations += u32::from(activated_a) + u32::from(activated_b);
        if !activated_a && !activated_b {
            break;
        }
    }

    assert_eq!(activations, MAX_REACTION_DEPTH + 1);
    assert!(
        !restarted
            .activate_now(&cycle_a)
            .await
            .expect("CycleA drained")
    );
    assert!(
        !restarted
            .activate_now(&cycle_b)
            .await
            .expect("CycleB drained")
    );
    let a_state = restarted
        .get_spec_actor_state(&cycle_a)
        .await
        .expect("CycleA state");
    let b_state = restarted
        .get_spec_actor_state(&cycle_b)
        .await
        .expect("CycleB state");
    assert_eq!(a_state.counters["visits"], 5);
    assert_eq!(b_state.counters["visits"], 4);
}
