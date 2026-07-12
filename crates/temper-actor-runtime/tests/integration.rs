//! Integration tests for the actor runtime against real Postgres.

use std::sync::Arc;

use temper_actor_runtime::*;
use testcontainers::{ContainerAsync, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use uuid::Uuid;

static POSTGRES: OnceCell<ContainerAsync<Postgres>> = OnceCell::const_new();
static LOCAL_SCHEMA: OnceCell<()> = OnceCell::const_new();

// ─── Proto messages (prost derive) ───────────────────────────────────────────

#[derive(Clone, PartialEq, prost::Message)]
pub struct StartMessage {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PingMessage {
    #[prost(string, tag = "1")]
    pub payload: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PongMessage {
    #[prost(string, tag = "1")]
    pub payload: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GenericMessage {
    #[prost(string, tag = "1")]
    pub content: String,
}

// ─── Test setup ──────────────────────────────────────────────────────────────

async fn test_pool() -> deadpool_postgres::Pool {
    if let Ok(database_url) = std::env::var("TEMPER_TEST_DATABASE_URL") {
        let mut config = deadpool_postgres::Config::new();
        config.url = Some(database_url);
        let pool = config
            .create_pool(Some(deadpool_postgres::Runtime::Tokio1), NoTls)
            .expect("local PostgreSQL pool");
        LOCAL_SCHEMA
            .get_or_init(|| async {
                let mut client = pool.get().await.expect("local schema client");
                temper_actor_runtime::schema::create_tables(&mut client)
                    .await
                    .expect("local actor schema");
            })
            .await;
        return pool;
    }

    let pg = POSTGRES
        .get_or_init(|| async {
            let container = Postgres::default()
                .start()
                .await
                .expect("failed to start postgres");
            let host = container.get_host().await.expect("get host");
            let port = container.get_host_port_ipv4(5432).await.expect("get port");
            let connstr = format!(
                "host={} port={} user={} password={} dbname={}",
                host, port, "postgres", "postgres", "postgres"
            );
            let (mut client, conn) = tokio_postgres::connect(&connstr, NoTls)
                .await
                .expect("connect failed");
            tokio::spawn(conn);
            temper_actor_runtime::schema::create_tables(&mut client)
                .await
                .expect("schema failed");
            container
        })
        .await;

    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(pg.get_host().await.expect("get host").to_string());
    cfg.port = Some(pg.get_host_port_ipv4(5432).await.expect("get port"));
    cfg.user = Some("postgres".to_string());
    cfg.password = Some("postgres".to_string());
    cfg.dbname = Some("postgres".to_string());
    cfg.create_pool(Some(deadpool_postgres::Runtime::Tokio1), NoTls)
        .expect("pool failed")
}

// ─── Test Actors ─────────────────────────────────────────────────────────────

struct PingActor {
    pong_type: String,
    count: u32,
}

#[async_trait::async_trait]
impl Actor for PingActor {
    fn actor_type(&self) -> &str {
        "Ping"
    }
    fn initial_state(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"sent": 0, "received": 0, "received_payloads": []}))
            .unwrap()
    }
    async fn handle(
        &self,
        ctx: &ActorContext,
        state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        let parse = serde_json::from_slice::<serde_json::Value>(state);
        assert!(
            parse.is_ok(),
            "PingActor state deser failed: {:?}",
            parse.err()
        );
        let mut s = parse.unwrap();

        if message.is::<StartMessage>() {
            let pong = ctx.spawn(&self.pong_type).await?;
            for i in 0..self.count {
                ctx.tell(
                    &pong,
                    PingMessage {
                        payload: format!("ping-{i}"),
                    },
                )
                .await;
            }
            s["sent"] = serde_json::json!(self.count);
        } else if message.is::<PongMessage>() {
            let pong = message
                .decode::<PongMessage>()
                .expect("decode PongMessage failed");
            let received = s["received"].as_u64().unwrap_or(0) + 1;
            s["received"] = serde_json::json!(received);
            s["received_payloads"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!(pong.payload));
        }

        *state = serde_json::to_vec(&s).unwrap();
        Ok(())
    }
}

struct PongActor;

#[async_trait::async_trait]
impl Actor for PongActor {
    fn actor_type(&self) -> &str {
        "Pong"
    }
    fn initial_state(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"received": 0, "received_payloads": []})).unwrap()
    }
    async fn handle(
        &self,
        ctx: &ActorContext,
        state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        let parse = serde_json::from_slice::<serde_json::Value>(state);
        assert!(
            parse.is_ok(),
            "PongActor state deser failed: {:?}",
            parse.err()
        );
        let mut s = parse.unwrap();

        if message.is::<PingMessage>() {
            let ping = message
                .decode::<PingMessage>()
                .expect("decode PingMessage failed");
            let received = s["received"].as_u64().unwrap_or(0) + 1;
            s["received"] = serde_json::json!(received);
            s["received_payloads"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!(ping.payload));

            if let Some(from) = &message.from {
                ctx.tell(
                    from,
                    PongMessage {
                        payload: format!("re:{}", ping.payload),
                    },
                )
                .await;
            }
        }

        *state = serde_json::to_vec(&s).unwrap();
        Ok(())
    }
}

struct EchoActor {
    name: String,
}

#[async_trait::async_trait]
impl Actor for EchoActor {
    fn actor_type(&self) -> &str {
        &self.name
    }
    fn initial_state(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"count": 0, "payloads": []})).unwrap()
    }
    async fn handle(
        &self,
        _ctx: &ActorContext,
        state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        let parse = serde_json::from_slice::<serde_json::Value>(state);
        assert!(
            parse.is_ok(),
            "EchoActor state deser failed: {:?}",
            parse.err()
        );
        let mut s = parse.unwrap();
        s["count"] = serde_json::json!(s["count"].as_u64().unwrap_or(0) + 1);
        let msg = message
            .decode::<GenericMessage>()
            .expect("decode GenericMessage failed");
        s["payloads"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(msg.content));
        *state = serde_json::to_vec(&s).unwrap();
        Ok(())
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn load_state(
    pool: &deadpool_postgres::Pool,
    namespace: &str,
    actor_type: &str,
) -> serde_json::Value {
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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_tell_and_activate() {
    let pool = test_pool().await;
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    let name = format!("echo_{}", Uuid::new_v4());
    system
        .register(Arc::new(EchoActor { name: name.clone() }))
        .await
        .unwrap();

    let namespace = format!("session/{}", Uuid::new_v4());
    let handle = system.spawn(&namespace, &name).await.unwrap();
    system
        .tell(
            None,
            &handle,
            GenericMessage {
                content: "world".into(),
            },
        )
        .await
        .unwrap();

    run_until_quiescent(&system, 10).await;

    let state = load_state(&pool, &namespace, &name).await;
    assert_eq!(state["count"], 1);
    assert_eq!(state["payloads"], serde_json::json!(["world"]));
}

#[tokio::test]
async fn test_ping_pong_3_rounds() {
    let pool = test_pool().await;
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    system
        .register(Arc::new(PingActor {
            pong_type: "Pong".into(),
            count: 3,
        }))
        .await
        .unwrap();
    system.register(Arc::new(PongActor)).await.unwrap();

    let namespace = format!("session/{}", Uuid::new_v4());
    let ping = system.spawn(&namespace, "Ping").await.unwrap();
    system.tell(None, &ping, StartMessage {}).await.unwrap();

    run_until_quiescent(&system, 30).await;

    let ping_state = load_state(&pool, &namespace, "Ping").await;
    assert_eq!(ping_state["sent"], 3);
    assert_eq!(ping_state["received"], 3);
    let ping_payloads: Vec<String> = ping_state["received_payloads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(ping_payloads, vec!["re:ping-0", "re:ping-1", "re:ping-2"]);

    let pong_state = load_state(&pool, &namespace, "Pong").await;
    assert_eq!(pong_state["received"], 3);
    let pong_payloads: Vec<String> = pong_state["received_payloads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(pong_payloads, vec!["ping-0", "ping-1", "ping-2"]);
}

#[tokio::test]
async fn test_no_pending_messages_skip() {
    let pool = test_pool().await;
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    let name = format!("echo_{}", Uuid::new_v4());
    system
        .register(Arc::new(EchoActor { name: name.clone() }))
        .await
        .unwrap();

    let namespace = format!("session/{}", Uuid::new_v4());
    system.spawn(&namespace, &name).await.unwrap();

    assert_eq!(system.poll_once().await.unwrap(), 0);
}

#[tokio::test]
async fn test_no_reprocessing() {
    let pool = test_pool().await;
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    let name = format!("echo_{}", Uuid::new_v4());
    system
        .register(Arc::new(EchoActor { name: name.clone() }))
        .await
        .unwrap();

    let namespace = format!("session/{}", Uuid::new_v4());
    let handle = system.spawn(&namespace, &name).await.unwrap();
    system
        .tell(
            None,
            &handle,
            GenericMessage {
                content: "test".into(),
            },
        )
        .await
        .unwrap();

    run_until_quiescent(&system, 10).await;

    assert_eq!(system.poll_once().await.unwrap(), 0);

    let state = load_state(&pool, &namespace, &name).await;
    assert_eq!(state["count"], 1, "should not reprocess");
    assert_eq!(state["payloads"], serde_json::json!(["test"]));
}

#[tokio::test]
async fn test_fifo_ordering() {
    let pool = test_pool().await;

    struct OrderActor {
        name: String,
    }
    #[async_trait::async_trait]
    impl Actor for OrderActor {
        fn actor_type(&self) -> &str {
            &self.name
        }
        fn initial_state(&self) -> Vec<u8> {
            b"[]".to_vec()
        }
        async fn handle(
            &self,
            _ctx: &ActorContext,
            state: &mut Vec<u8>,
            message: &Message,
        ) -> Result<(), ActorError> {
            let parse = serde_json::from_slice::<Vec<String>>(state);
            assert!(
                parse.is_ok(),
                "OrderActor state deser failed: {:?}",
                parse.err()
            );
            let mut order = parse.unwrap();
            let msg = message.decode::<GenericMessage>().expect("decode failed");
            order.push(msg.content);
            *state = serde_json::to_vec(&order).unwrap();
            Ok(())
        }
    }

    let name = format!("order_{}", Uuid::new_v4());
    let system = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    system
        .register(Arc::new(OrderActor { name: name.clone() }))
        .await
        .unwrap();

    let namespace = format!("session/{}", Uuid::new_v4());
    let handle = system.spawn(&namespace, &name).await.unwrap();

    for i in 0..5 {
        system
            .tell(
                None,
                &handle,
                GenericMessage {
                    content: format!("msg-{i}"),
                },
            )
            .await
            .unwrap();
    }

    run_until_quiescent(&system, 20).await;

    let state = load_state(&pool, &namespace, &name).await;
    let order: Vec<String> = state
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(order, vec!["msg-0", "msg-1", "msg-2", "msg-3", "msg-4"]);
}
