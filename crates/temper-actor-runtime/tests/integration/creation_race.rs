use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
use tokio::sync::Notify;

struct ConcurrentCreation {
    name: String,
    first: AtomicBool,
    observed_absence: Arc<Notify>,
    proceed: Mutex<mpsc::Receiver<()>>,
}

#[async_trait::async_trait]
impl Actor for ConcurrentCreation {
    fn actor_type(&self) -> &str {
        &self.name
    }
    fn initial_state(&self) -> Vec<u8> {
        serde_json::to_vec(&SpecActorState::default()).unwrap()
    }
    fn initial_state_for(&self, _handle: &ActorHandle) -> Vec<u8> {
        if self.first.swap(false, Ordering::SeqCst) {
            self.observed_absence.notify_one();
            tokio::task::block_in_place(|| {
                self.proceed
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .expect("concurrent creation completes")
            });
        }
        self.initial_state()
    }
    async fn handle(
        &self,
        _ctx: &ActorContext,
        state: &mut Vec<u8>,
        _message: &Message,
    ) -> Result<(), ActorError> {
        let mut value: SpecActorState = serde_json::from_slice(state).unwrap();
        value.fields["handled"] = serde_json::json!(true);
        *state = serde_json::to_vec(&value).unwrap();
        Ok(())
    }
}

/// Prove activation preserves fields committed by concurrent actor creation.
#[cfg_attr(
    not(feature = "test-shared-pg-proofs"),
    tokio::test(flavor = "multi_thread", worker_threads = 3)
)]
pub async fn activation_preserves_fields_from_concurrent_creation() {
    let pool = test_pool().await;
    let system = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    let name = format!("creation_race_{}", Uuid::new_v4());
    let handle = ActorHandle::new(format!("race/{}", Uuid::new_v4()), name.clone());
    let observed = Arc::new(Notify::new());
    let (proceed, wait) = mpsc::channel();
    system
        .register(Arc::new(ConcurrentCreation {
            name,
            first: AtomicBool::new(true),
            observed_absence: observed.clone(),
            proceed: Mutex::new(wait),
        }))
        .await
        .unwrap();
    system
        .tell(
            None,
            &handle,
            GenericMessage {
                content: "trigger".into(),
            },
        )
        .await
        .unwrap();
    let activation = {
        let system = system.clone();
        let handle = handle.clone();
        tokio::spawn(async move { system.activate_now(&handle).await })
    };
    tokio::time::timeout(Duration::from_secs(10), observed.notified())
        .await
        .unwrap();
    system
        .spawn_with_fields(
            &handle.namespace,
            &handle.actor_type,
            serde_json::json!({"marker":"concurrently committed"}),
        )
        .await
        .unwrap();
    proceed.send(()).unwrap();
    assert!(activation.await.unwrap().unwrap());
    let row = system
        .load_state(&handle.namespace, &handle.actor_type)
        .await
        .unwrap()
        .unwrap();
    let state: SpecActorState = serde_json::from_slice(&row).unwrap();
    assert_eq!(state.fields["marker"], "concurrently committed");
    assert_eq!(state.fields["handled"], true);
}
