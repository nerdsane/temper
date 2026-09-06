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
