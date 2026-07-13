//! PostgreSQL-backed behavioral coverage for canonical effect execution.

use std::sync::Arc;
use std::time::Duration;

use temper_actor_runtime::{ActorSystem, SchedulerConfig, SpecDrivenActor, SpecMessage, schema};
use temper_runtime::reaction::ReactionRegistry;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use uuid::Uuid;

static LOCAL_SCHEMA: OnceCell<()> = OnceCell::const_new();

const PARENT_SPEC: &str = r#"
[automaton]
name = "EffectParent"
states = ["Idle", "Collected", "Done", "Woken"]
initial = "Idle"

[[state]]
name = "entries"
type = "list"
initial = "[]"

[[action]]
name = "Append"
kind = "input"
from = ["Idle"]
to = "Collected"
effect = [{ type = "list_append", var = "entries" }]

[[action]]
name = "Run"
kind = "input"
from = ["Collected"]
to = "Done"
guard = [{ type = "list_length_min", var = "entries", min = 1 }]
effect = [
  { type = "schedule", action = "Wake", delay_seconds = 0 },
  { type = "spawn", entity_type = "EffectChild", entity_id_source = "child_id", initial_action = "Start", store_id_in = "spawned_id", copy_fields = "owner" },
]

[[action]]
name = "Wake"
kind = "input"
from = ["Done"]
to = "Woken"
"#;

const CHILD_SPEC: &str = r#"
[automaton]
name = "EffectChild"
states = ["Idle", "Active"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Active"
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

async fn register_effect_actors(system: &ActorSystem) {
    system
        .register(Arc::new(
            SpecDrivenActor::from_ioa(PARENT_SPEC, ReactionRegistry::new()).expect("parent spec"),
        ))
        .await
        .expect("register parent");
    system
        .register(Arc::new(
            SpecDrivenActor::from_ioa(CHILD_SPEC, ReactionRegistry::new()).expect("child spec"),
        ))
        .await
        .expect("register child");
}

async fn promote_until_namespace_remaining(
    client: &deadpool_postgres::Client,
    namespace: &str,
    expected_remaining: i64,
) {
    for _ in 0..1_000 {
        let remaining: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM odp_temper.actor_scheduled_messages WHERE namespace = $1",
                &[&namespace],
            )
            .await
            .expect("count namespace timers")
            .get(0);
        if remaining == expected_remaining {
            return;
        }
        assert!(remaining > expected_remaining);
        assert_eq!(
            client
                .execute(schema::PROMOTE_DUE_MESSAGES, &[&1_i64])
                .await
                .expect("promote due timer"),
            1,
            "due timers must make bounded promotion progress"
        );
    }
    panic!("timer promotion budget exhausted before namespace reached expected count");
}

#[tokio::test]
async fn postgres_effects_survive_restart_and_commit_runtime_commands() {
    let (_container, pool) = postgres_pool().await;
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    register_effect_actors(&system).await;

    let namespace = format!("default/{}", Uuid::new_v4());
    let child_id = format!("child-{}", Uuid::new_v4());
    let parent = system
        .spawn_with_fields(
            &namespace,
            "EffectParent",
            serde_json::json!({"owner": "owner-1"}),
        )
        .await
        .expect("spawn parent");
    system
        .tell(
            None,
            &parent,
            SpecMessage::with_params("Append", serde_json::json!({"entries": "durable-value"})),
        )
        .await
        .expect("enqueue append");
    assert!(system.activate_now(&parent).await.expect("activate append"));

    let persisted = system
        .get_spec_actor_state(&parent)
        .await
        .expect("persisted parent state");
    assert_eq!(persisted.lists["entries"], ["durable-value"]);

    let restarted = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    register_effect_actors(&restarted).await;
    restarted
        .tell(
            None,
            &parent,
            SpecMessage::with_params("Run", serde_json::json!({"child_id": child_id.clone()})),
        )
        .await
        .expect("enqueue guarded action after restart");
    assert!(
        restarted
            .activate_now(&parent)
            .await
            .expect("activate guarded action")
    );

    let parent_state = restarted
        .get_spec_actor_state(&parent)
        .await
        .expect("parent state after guarded action");
    assert_eq!(parent_state.status, "Done");
    assert_eq!(parent_state.fields["spawned_id"], child_id);

    let child =
        temper_actor_runtime::ActorHandle::new(format!("default/{child_id}"), "EffectChild");
    assert!(
        restarted
            .activate_now(&child)
            .await
            .expect("activate spawned child")
    );
    let child_state = restarted
        .get_spec_actor_state(&child)
        .await
        .expect("spawned child state");
    assert_eq!(child_state.status, "Active");
    assert_eq!(child_state.fields["owner"], "owner-1");

    let client = pool.get().await.expect("promotion client");
    promote_until_namespace_remaining(&client, &parent.namespace, 0).await;
    assert!(
        restarted
            .activate_now(&parent)
            .await
            .expect("activate promoted timer")
    );
    let woken = restarted
        .get_spec_actor_state(&parent)
        .await
        .expect("woken parent state");
    assert_eq!(woken.status, "Woken");

    let from_namespace: Option<String> = None;
    let from_actor: Option<String> = None;
    let correlation_id: Option<Uuid> = None;
    for sequence in 0_u8..2 {
        client
            .query_one(
                schema::INSERT_SCHEDULED_MESSAGE,
                &[
                    &parent.namespace,
                    &parent.actor_type,
                    &from_namespace,
                    &from_actor,
                    &"SpecMessage",
                    &vec![sequence],
                    &correlation_id,
                    &chrono::Utc::now(),
                ],
            )
            .await
            .expect("insert batch-boundary timer");
    }
    promote_until_namespace_remaining(&client, &parent.namespace, 1).await;
    let remaining: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM odp_temper.actor_scheduled_messages WHERE namespace = $1",
            &[&parent.namespace],
        )
        .await
        .expect("count remaining timers")
        .get(0);
    assert_eq!(remaining, 1);
    promote_until_namespace_remaining(&client, &parent.namespace, 0).await;
}

#[tokio::test]
async fn command_flush_failure_rolls_back_parent_state_cursor_and_commands() {
    let (_container, pool) = postgres_pool().await;
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    system
        .register(Arc::new(
            SpecDrivenActor::from_ioa(PARENT_SPEC, ReactionRegistry::new()).expect("parent spec"),
        ))
        .await
        .expect("register parent without child handler");

    let namespace = format!("default/{}", Uuid::new_v4());
    let child_id = format!("child-{}", Uuid::new_v4());
    let child_namespace = format!("default/{child_id}");
    let parent = system
        .spawn_with_fields(
            &namespace,
            "EffectParent",
            serde_json::json!({"owner": "owner-1"}),
        )
        .await
        .expect("spawn parent");
    system
        .tell(
            None,
            &parent,
            SpecMessage::with_params("Append", serde_json::json!({"entries": "durable-value"})),
        )
        .await
        .expect("enqueue append");
    assert!(system.activate_now(&parent).await.expect("activate append"));

    let client = pool.get().await.expect("rollback assertion client");
    let before = client
        .query_one(
            "SELECT state, last_msg_id, version FROM odp_temper.actor_instances WHERE namespace = $1 AND actor_type = $2",
            &[&parent.namespace, &parent.actor_type],
        )
        .await
        .expect("parent before failure");
    let before_state: Vec<u8> = before.get("state");
    let before_cursor: i64 = before.get("last_msg_id");
    let before_version: i64 = before.get("version");

    system
        .tell(
            None,
            &parent,
            SpecMessage::with_params("Run", serde_json::json!({"child_id": child_id})),
        )
        .await
        .expect("enqueue command-producing action");
    let error = system
        .activate_now(&parent)
        .await
        .expect_err("unregistered child handler must fail the activation");
    assert!(error.to_string().contains("not registered"));

    let after = client
        .query_one(
            "SELECT state, last_msg_id, version FROM odp_temper.actor_instances WHERE namespace = $1 AND actor_type = $2",
            &[&parent.namespace, &parent.actor_type],
        )
        .await
        .expect("parent after failure");
    assert_eq!(after.get::<_, Vec<u8>>("state"), before_state);
    assert_eq!(after.get::<_, i64>("last_msg_id"), before_cursor);
    assert_eq!(after.get::<_, i64>("version"), before_version);

    let scheduled: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM odp_temper.actor_scheduled_messages WHERE namespace = $1",
            &[&parent.namespace],
        )
        .await
        .expect("scheduled rollback count")
        .get(0);
    let children: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM odp_temper.actor_instances WHERE namespace = $1 AND actor_type = 'EffectChild'",
            &[&child_namespace],
        )
        .await
        .expect("child rollback count")
        .get(0);
    let child_messages: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM odp_temper.actor_messages WHERE namespace = $1 AND to_actor = 'EffectChild'",
            &[&child_namespace],
        )
        .await
        .expect("child-message rollback count")
        .get(0);
    assert_eq!((scheduled, children, child_messages), (0, 0, 0));
}

#[tokio::test]
async fn duplicate_durable_spawn_enqueues_initial_action_once() {
    let (_container, pool) = postgres_pool().await;
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    register_effect_actors(&system).await;

    let child_id = format!("shared-child-{}", Uuid::new_v4());
    let child_namespace = format!("default/{child_id}");
    for parent_id in [Uuid::new_v4(), Uuid::new_v4()] {
        let parent = system
            .spawn_with_fields(
                &format!("default/{parent_id}"),
                "EffectParent",
                serde_json::json!({"owner": "owner-1"}),
            )
            .await
            .expect("spawn parent");
        system
            .tell(
                None,
                &parent,
                SpecMessage::with_params("Append", serde_json::json!({"entries": "durable-value"})),
            )
            .await
            .expect("enqueue append");
        assert!(system.activate_now(&parent).await.expect("activate append"));
        system
            .tell(
                None,
                &parent,
                SpecMessage::with_params("Run", serde_json::json!({"child_id": child_id.clone()})),
            )
            .await
            .expect("enqueue spawn action");
        assert!(
            system
                .activate_now(&parent)
                .await
                .expect("activate spawn action")
        );
    }

    let client = pool.get().await.expect("duplicate-spawn assertion client");
    let actor_count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM odp_temper.actor_instances WHERE namespace = $1 AND actor_type = 'EffectChild'",
            &[&child_namespace],
        )
        .await
        .expect("count durable child actors")
        .get(0);
    let initial_message_count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM odp_temper.actor_messages WHERE namespace = $1 AND to_actor = 'EffectChild'",
            &[&child_namespace],
        )
        .await
        .expect("count durable child initial messages")
        .get(0);

    assert_eq!(actor_count, 1, "duplicate spawn must remain idempotent");
    assert_eq!(
        initial_message_count, 1,
        "an existing durable child must not receive the initial action again"
    );
}

#[tokio::test]
async fn concurrent_timer_promoters_preserve_global_due_order() {
    let (_container, pool) = postgres_pool().await;
    let namespace = format!("timer-order/{}", Uuid::new_v4());
    let from_namespace: Option<String> = None;
    let from_actor: Option<String> = None;
    let correlation_id: Option<Uuid> = None;
    let insert_client = pool.get().await.expect("timer insert client");
    for sequence in 0_u8..2 {
        insert_client
            .query_one(
                schema::INSERT_SCHEDULED_MESSAGE,
                &[
                    &namespace,
                    &"TimerTarget",
                    &from_namespace,
                    &from_actor,
                    &"SpecMessage",
                    &vec![sequence],
                    &correlation_id,
                    &chrono::Utc::now(),
                ],
            )
            .await
            .expect("insert ordered timer");
    }

    let mut first_client = pool.get().await.expect("first promoter client");
    let first_tx = first_client
        .build_transaction()
        .start()
        .await
        .expect("first promoter transaction");
    assert_eq!(
        first_tx
            .execute(schema::PROMOTE_DUE_MESSAGES, &[&1_i64])
            .await
            .expect("first promotion"),
        1
    );

    let second_pool = pool.clone();
    let mut second = tokio::spawn(async move {
        let client = second_pool.get().await.expect("second promoter client");
        client
            .execute(schema::PROMOTE_DUE_MESSAGES, &[&1_i64])
            .await
            .expect("second promotion")
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut second)
            .await
            .is_err(),
        "a concurrent promoter must wait instead of skipping the globally oldest locked timer"
    );
    first_tx.commit().await.expect("commit first promotion");
    assert_eq!(second.await.expect("join second promoter"), 1);

    let assert_client = pool.get().await.expect("timer assertion client");
    let payloads: Vec<Vec<u8>> = assert_client
        .query(
            "SELECT payload FROM odp_temper.actor_messages WHERE namespace = $1 AND to_actor = 'TimerTarget' ORDER BY id",
            &[&namespace],
        )
        .await
        .expect("ordered promoted messages")
        .into_iter()
        .map(|row| row.get("payload"))
        .collect();
    assert_eq!(payloads, [vec![0], vec![1]]);
}

#[tokio::test]
async fn concurrent_schema_initializers_wait_for_the_global_ddl_lock() {
    let (_container, pool) = postgres_pool().await;
    let mut first_client = pool.get().await.expect("schema lock holder");
    let first_tx = first_client
        .build_transaction()
        .start()
        .await
        .expect("schema lock transaction");
    first_tx
        .batch_execute(
            "SELECT pg_advisory_xact_lock(hashtext('odp_temper.actor_schema_initialization'))",
        )
        .await
        .expect("hold schema initialization lock");

    let second_pool = pool.clone();
    let mut second = tokio::spawn(async move {
        let mut client = second_pool.get().await.expect("second schema client");
        schema::create_tables(&mut client).await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut second)
            .await
            .is_err(),
        "concurrent schema initialization must wait for the global DDL lock"
    );
    first_tx.commit().await.expect("release schema lock");
    second
        .await
        .expect("join second schema initializer")
        .expect("second schema initialization");
}
