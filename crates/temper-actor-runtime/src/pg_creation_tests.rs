//! Public actor creation contract, exercised against PostgreSQL.
use super::*;

#[tokio::test]
async fn public_spawn_with_fields_validates_strict_creation_before_writing() {
    let (pool, _container) = pool().await;
    let system = crate::ActorSystem::new(pool.clone(), crate::SchedulerConfig::default());
    system
        .register(Arc::new(
            SpecDrivenActor::from_ioa(SPEC, HashMap::new()).unwrap(),
        ))
        .await
        .unwrap();
    let namespace = format!("creation-{}", Uuid::new_v4());
    let handle = ActorHandle::new(&namespace, "Strict");
    assert!(matches!(
        system
            .spawn_with_fields(
                &namespace,
                "Strict",
                serde_json::json!({
                    "desired":"forged"
                })
            )
            .await,
        Err(ActorError::Rejected(_))
    ));
    assert!(
        pool.get()
            .await
            .unwrap()
            .query(schema::LOAD_ACTOR, &[&handle.namespace, &handle.actor_type])
            .await
            .unwrap()
            .is_empty()
    );
    system
        .spawn_with_fields(&namespace, "Strict", serde_json::json!({"Id":"canonical"}))
        .await
        .unwrap();
    let before = read(&pool, &handle).await;
    let state: SpecActorState = serde_json::from_slice(&before.0).unwrap();
    assert_eq!(state.fields["desired"], "first");
    assert_eq!(state.fields["Id"], "canonical");
    assert!(matches!(
        system
            .spawn_with_fields(
                &namespace,
                "Strict",
                serde_json::json!({
                    "desired":"forged-again"
                })
            )
            .await,
        Err(ActorError::Rejected(_))
    ));
    assert_eq!(read(&pool, &handle).await, before);
}

struct ChildSpawner;

#[async_trait::async_trait]
impl Actor for ChildSpawner {
    fn actor_type(&self) -> &str {
        "ChildSpawner"
    }

    fn initial_state(&self) -> Vec<u8> {
        b"{}".to_vec()
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        _state: &mut Vec<u8>,
        _message: &Message,
    ) -> Result<(), ActorError> {
        let child = ctx.spawn("Strict").await?;
        assert_eq!(ctx.lookup("Strict").await?, Some(child));
        assert!(matches!(
            ctx.spawn("UnregisteredChild").await,
            Err(ActorError::NotFound(_))
        ));
        assert_eq!(ctx.lookup("UnregisteredChild").await?, None);
        Ok(())
    }
}

#[tokio::test]
async fn context_spawn_persists_strict_child_defaults_before_activation() {
    let (pool, _container) = pool().await;
    let system = crate::ActorSystem::new(pool.clone(), crate::SchedulerConfig::default());
    // Registration after system construction must reach contexts through the same registry.
    system.register(Arc::new(ChildSpawner)).await.unwrap();
    let spec = SPEC.replace(
        "[[action]]",
        "[[state]]\nname = \"attempts\"\ntype = \"counter\"\ninitial = \"5\"\n[[action]]",
    );
    system
        .register(Arc::new(
            SpecDrivenActor::from_ioa(&spec, HashMap::new()).unwrap(),
        ))
        .await
        .unwrap();
    let namespace = format!("context-child-{}", Uuid::new_v4());
    let parent = system.spawn(&namespace, "ChildSpawner").await.unwrap();
    system
        .tell(None, &parent, SpecMessage::new("Spawn"))
        .await
        .unwrap();
    system.activate_now(&parent).await.unwrap();
    let child = ActorHandle::new(&namespace, "Strict");
    let initial = read(&pool, &child).await;
    assert!(
        !initial.0.is_empty(),
        "context spawn must persist initial bytes immediately"
    );
    let state: SpecActorState = serde_json::from_slice(&initial.0).unwrap();
    assert_eq!(state.fields["Id"], namespace);
    assert_eq!(state.fields["id"], namespace);
    assert_eq!(state.fields["desired"], "first");
    assert_eq!(state.counters["attempts"], 5);
    assert_eq!((initial.1, initial.2), (0, 0));
    system
        .tell(
            None,
            &child,
            SpecMessage::with_params(
                "Replace",
                serde_json::json!({"desired":"changed","expected_desired":"first"}),
            ),
        )
        .await
        .unwrap();
    system.activate_now(&child).await.unwrap();
    let changed = read(&pool, &child).await;
    let state: SpecActorState = serde_json::from_slice(&changed.0).unwrap();
    assert_eq!(state.fields["desired"], "changed");
    assert_eq!(state.counters["attempts"], 5);
    system
        .tell(None, &parent, SpecMessage::new("Spawn"))
        .await
        .unwrap();
    system.activate_now(&parent).await.unwrap();
    assert_eq!(
        read(&pool, &child).await,
        changed,
        "repeated spawn preserves the child"
    );
}
