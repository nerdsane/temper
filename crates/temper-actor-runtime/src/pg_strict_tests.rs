//! Real Postgres proofs for rejection consumption and retryable rollback.
use super::*;
use crate::spec_actor::{SpecActorState, SpecDrivenActor, SpecMessage};
use prost::Message as _;
use std::collections::HashMap;

static SCHEMA_READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn pool() -> (
    Pool,
    Option<testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>>,
) {
    if let Ok(url) = std::env::var("TEMPER_ACTOR_TEST_DATABASE_URL") {
        let parsed: tokio_postgres::Config = url.parse().unwrap();
        assert!(
            parsed.get_hosts().iter().all(|host| matches!(host,
            tokio_postgres::config::Host::Tcp(name) if name == "127.0.0.1" || name == "localhost"))
        );
        assert!(
            parsed
                .get_dbname()
                .is_some_and(|name| name.starts_with("temper_test_"))
        );
        let mut config = deadpool_postgres::Config::new();
        config.url = Some(url);
        let pool = config
            .create_pool(
                Some(deadpool_postgres::Runtime::Tokio1),
                tokio_postgres::NoTls,
            )
            .unwrap();
        SCHEMA_READY
            .get_or_init(|| async {
                schema::create_tables(&pool.get().await.unwrap())
                    .await
                    .unwrap();
            })
            .await;
        (pool, None)
    } else {
        let (pool, container) = crate::test_utils::setup_test_pg().await;
        (pool, Some(container))
    }
}

async fn read(pool: &Pool, handle: &ActorHandle) -> (Vec<u8>, i64, i64) {
    let row = pool
        .get()
        .await
        .unwrap()
        .query_one(schema::LOAD_ACTOR, &[&handle.namespace, &handle.actor_type])
        .await
        .unwrap();
    (row.get("state"), row.get("last_msg_id"), row.get("version"))
}

async fn setup(pool: &Pool, handler: &dyn Actor) -> ActorHandle {
    let handle = ActorHandle::new(format!("strict-{}", Uuid::new_v4()), handler.actor_type());
    pool.get()
        .await
        .unwrap()
        .execute(
            schema::CREATE_ACTOR,
            &[
                &handle.namespace,
                &handle.actor_type,
                &handler.initial_state(),
            ],
        )
        .await
        .unwrap();
    handle
}

const SPEC: &str = r#"
[automaton]
name = "Strict"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
[[state]]
name = "desired"
type = "string"
initial = "first"
[[action]]
name = "Replace"
kind = "input"
from = ["Ready"]
params = ["desired", "expected_desired"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected_desired"
field = "desired"
"#;

#[tokio::test]
async fn rejected_input_is_consumed_and_the_next_valid_message_runs() {
    let (pool, _container) = pool().await;
    let actor = SpecDrivenActor::from_ioa(SPEC, HashMap::new()).unwrap();
    let handle = setup(&pool, &actor).await;
    let mailbox = Arc::new(PgMailbox::new(pool.clone(), PgMailboxConfig::default()));
    let activator = PgActorActivator::new(pool.clone(), mailbox.clone());
    let initial = read(&pool, &handle).await.0;
    let mut rejected_ids = Vec::new();
    for (kind, payload) in [
        ("SpecMessage", vec![0xff]),
        (
            "Replace",
            br#"{"desired":"bad","expected_desired":"stale"}"#.to_vec(),
        ),
        (
            "Replace",
            br#"{"desired":"bad","expected_desired":"first","extra":true}"#.to_vec(),
        ),
    ] {
        let id = mailbox.tell(None, &handle, kind, payload).await.unwrap();
        rejected_ids.push(id);
    }
    let id = mailbox
        .tell(
            None,
            &handle,
            "SpecMessage",
            SpecMessage::with_params(
                "Replace",
                serde_json::json!({"desired": "second", "expected_desired": "first"}),
            )
            .encode_to_vec(),
        )
        .await
        .unwrap();
    // The valid request is already queued behind all three rejected requests.
    for id in rejected_ids {
        assert!(matches!(
            activator.activate(&handle, &actor).await,
            Err(ActivationError::ActorError(ActorError::Rejected(_)))
        ));
        let (bytes, cursor, _) = read(&pool, &handle).await;
        assert_eq!(bytes, initial);
        assert_eq!(cursor, id);
    }
    assert!(activator.activate(&handle, &actor).await.unwrap().activated);
    let (bytes, cursor, _) = read(&pool, &handle).await;
    assert_eq!(cursor, id);
    let state: SpecActorState = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(state.fields["desired"], "second");
    assert!(!activator.activate(&handle, &actor).await.unwrap().activated);
}

struct MutatingFailure {
    rejected: bool,
    recovered: bool,
}

#[async_trait::async_trait]
impl Actor for MutatingFailure {
    fn actor_type(&self) -> &str {
        "MutatingFailure"
    }
    fn initial_state(&self) -> Vec<u8> {
        b"original bytes".to_vec()
    }
    async fn handle(
        &self,
        ctx: &ActorContext,
        state: &mut Vec<u8>,
        _: &Message,
    ) -> Result<(), ActorError> {
        *state = b"changed bytes".to_vec();
        ctx.tell(
            &ActorHandle::new(ctx.self_handle().namespace.clone(), "Audit"),
            SpecMessage::new("Record"),
        )
        .await;
        if self.recovered {
            return Ok(());
        }
        if self.rejected {
            Err(ActorError::Rejected("invalid input".into()))
        } else {
            Err(ActorError::HandlerFailed(
                "transient provider outage".into(),
            ))
        }
    }
}

#[tokio::test]
async fn rejection_discards_handler_mutations_and_tells_but_transient_failure_retries() {
    let (pool, _container) = pool().await;
    let mailbox = Arc::new(PgMailbox::new(pool.clone(), PgMailboxConfig::default()));
    let activator = PgActorActivator::new(pool.clone(), mailbox.clone());
    for rejected in [true, false] {
        let actor = MutatingFailure {
            rejected,
            recovered: false,
        };
        let handle = setup(&pool, &actor).await;
        let before = read(&pool, &handle).await;
        let id = mailbox
            .tell(None, &handle, "Request", vec![])
            .await
            .unwrap();
        assert!(activator.activate(&handle, &actor).await.is_err());
        let (bytes, cursor, version) = read(&pool, &handle).await;
        assert_eq!(bytes, before.0);
        assert_eq!(cursor, if rejected { id } else { before.1 });
        assert_eq!(version, before.2 + i64::from(rejected));
        let tell_count: i64 = pool.get().await.unwrap().query_one(
            "SELECT count(*) FROM odp_temper.actor_messages WHERE namespace = $1 AND to_actor = 'Audit'",
            &[&handle.namespace]).await.unwrap().get(0);
        assert_eq!(tell_count, 0);
        let recovered = MutatingFailure {
            rejected: false,
            recovered: true,
        };
        if rejected {
            mailbox
                .tell(None, &handle, "Request", vec![])
                .await
                .unwrap();
        }
        assert!(
            activator
                .activate(&handle, &recovered)
                .await
                .unwrap()
                .activated
        );
        assert_eq!(read(&pool, &handle).await.0, b"changed bytes");
    }
}

#[tokio::test]
async fn routed_emit_and_trigger_project_only_declared_inputs_then_enforce_constraints() {
    let (pool, _container) = pool().await;
    for effect in [
        r#"{type = "emit", event = "Forward"}"#,
        r#"{type = "trigger", name = "Forward"}"#,
    ] {
        let source_spec = format!(
            r#"
[automaton]
name = "Source"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
[[state]]
name = "source_only"
type = "string"
initial = "not a sink parameter"
[[action]]
name = "Forward"
kind = "input"
from = ["Ready"]
params = ["desired", "expected_desired"]
effect = [{effect}]
"#
        );
        let source = SpecDrivenActor::from_ioa(
            &source_spec,
            HashMap::from([("Forward".into(), ("Sink".into(), "Replace".into()))]),
        )
        .unwrap();
        let sink =
            SpecDrivenActor::from_ioa(&SPEC.replace("Strict", "Sink"), HashMap::new()).unwrap();
        let system = crate::ActorSystem::new(pool.clone(), crate::SchedulerConfig::default());
        system.register(Arc::new(source)).await.unwrap();
        system.register(Arc::new(sink)).await.unwrap();
        let namespace = format!("strict-route-{}", Uuid::new_v4());
        let source = system.spawn(&namespace, "Source").await.unwrap();
        let sink = system.spawn(&namespace, "Sink").await.unwrap();
        system
            .tell(
                None,
                &source,
                SpecMessage::with_params(
                    "Forward",
                    serde_json::json!({"desired":"second", "expected_desired":"first"}),
                ),
            )
            .await
            .unwrap();
        system.activate_now(&source).await.unwrap();
        system
            .activate_now(&sink)
            .await
            .expect("declared routed fields must reach the strict target");
        let accepted = read(&pool, &sink).await.0;
        let state: SpecActorState = serde_json::from_slice(&accepted).unwrap();
        assert_eq!(state.fields["desired"], "second");
        assert!(state.fields.get("source_only").is_none());
        system
            .tell(
                None,
                &source,
                SpecMessage::with_params(
                    "Forward",
                    serde_json::json!({"desired":"third", "expected_desired":"stale"}),
                ),
            )
            .await
            .unwrap();
        system.activate_now(&source).await.unwrap();
        assert!(matches!(
            system.activate_now(&sink).await,
            Err(ActivationError::ActorError(ActorError::Rejected(_)))
        ));
        assert_eq!(read(&pool, &sink).await.0, accepted);
        // Public callers cannot opt into internal projection by naming the envelope.
        system
            .tell(
                None,
                &sink,
                crate::spec_actor::RoutedSpecMessage::from(SpecMessage::with_params(
                    "Replace",
                    serde_json::json!({
                        "desired":"forged", "expected_desired":"second", "source_only":true
                    }),
                )),
            )
            .await
            .unwrap();
        assert!(matches!(
            system.activate_now(&sink).await,
            Err(ActivationError::ActorError(ActorError::Rejected(_)))
        ));
        assert_eq!(read(&pool, &sink).await.0, accepted);
        // An ordinary actor-origin request also keeps the exact public allowlist.
        system
            .tell(
                Some(&source),
                &sink,
                SpecMessage::with_params(
                    "Replace",
                    serde_json::json!({
                        "desired":"forged", "expected_desired":"second", "source_only":true
                    }),
                ),
            )
            .await
            .unwrap();
        assert!(matches!(
            system.activate_now(&sink).await,
            Err(ActivationError::ActorError(ActorError::Rejected(_)))
        ));
        assert_eq!(read(&pool, &sink).await.0, accepted);
    }
}

#[tokio::test]
async fn round_three_fresh_identity_is_persisted_before_any_action() {
    let (pool, _container) = pool().await;
    let spec = SPEC.replace("field = \"desired\"", "field = \"Id\"");
    let system = crate::ActorSystem::new(pool.clone(), crate::SchedulerConfig::default());
    system
        .register(Arc::new(
            SpecDrivenActor::from_ioa(&spec, HashMap::new()).unwrap(),
        ))
        .await
        .unwrap();
    for variant in 0..3 {
        let namespace = format!("identity-{variant}-{}", Uuid::new_v4());
        let expected = if variant == 2 {
            "explicit-http-id"
        } else {
            namespace.as_str()
        };
        let handle = match variant {
            0 => system.spawn(&namespace, "Strict").await.unwrap(),
            1 => {
                system.spawn_all_registered(&namespace).await.unwrap();
                ActorHandle::new(&namespace, "Strict")
            }
            _ => system
                .spawn_with_fields(
                    &namespace,
                    "Strict",
                    serde_json::json!({"Id":expected,"id":expected}),
                )
                .await
                .unwrap(),
        };
        let before = read(&pool, &handle).await;
        let state: SpecActorState = serde_json::from_slice(&before.0).unwrap();
        assert_eq!(state.fields["Id"], expected);
        assert_eq!(state.fields["id"], expected);
        assert_eq!(before.1, 0);
        system
            .tell(
                None,
                &handle,
                SpecMessage::with_params(
                    "Replace",
                    serde_json::json!({"desired":"changed","expected_desired":expected}),
                ),
            )
            .await
            .unwrap();
        system.activate_now(&handle).await.unwrap();
        let state: SpecActorState = serde_json::from_slice(&read(&pool, &handle).await.0).unwrap();
        assert_eq!(state.fields["desired"], "changed");
        system.spawn(&namespace, "Strict").await.unwrap();
        let retained: SpecActorState =
            serde_json::from_slice(&read(&pool, &handle).await.0).unwrap();
        assert_eq!(retained.fields["Id"], expected);
        assert_eq!(retained.fields["desired"], "changed");
    }
}

#[tokio::test]
async fn activation_preserves_recovered_bytes_and_initializes_only_absent_actors() {
    let (pool, _container) = pool().await;
    let mailbox = Arc::new(PgMailbox::new(pool.clone(), PgMailboxConfig::default()));
    let activator = PgActorActivator::new(pool.clone(), mailbox.clone());
    for recovered_empty in [true, false] {
        let spec = if recovered_empty {
            SPEC.to_owned()
        } else {
            SPEC.replace("field = \"desired\"", "field = \"Id\"")
        };
        let actor = SpecDrivenActor::from_ioa(&spec, HashMap::new()).unwrap();
        let handle = ActorHandle::new(format!("activation-{}", Uuid::new_v4()), "Strict");
        if recovered_empty {
            pool.get()
                .await
                .unwrap()
                .execute(
                    schema::CREATE_ACTOR,
                    &[&handle.namespace, &handle.actor_type, &Vec::<u8>::new()],
                )
                .await
                .unwrap();
        }
        let expected = if recovered_empty {
            "first"
        } else {
            &handle.namespace
        };
        let message_id = mailbox
            .tell(
                None,
                &handle,
                "SpecMessage",
                SpecMessage::with_params(
                    "Replace",
                    serde_json::json!({"desired":"accepted", "expected_desired":expected}),
                )
                .encode_to_vec(),
            )
            .await
            .unwrap();
        let result = activator.activate(&handle, &actor).await;
        let (bytes, cursor, version) = read(&pool, &handle).await;
        assert_eq!(cursor, message_id);
        if recovered_empty {
            assert!(matches!(
                result,
                Err(ActivationError::ActorError(ActorError::Rejected(_)))
            ));
            assert!(
                bytes.is_empty(),
                "recovery must not fabricate initial state"
            );
            assert_eq!(version, 1, "consuming the refusal advances the row version");
        } else {
            assert!(result.unwrap().activated);
            let state: SpecActorState = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(state.fields["Id"], handle.namespace);
            assert_eq!(state.fields["desired"], "accepted");
        }
    }
}
