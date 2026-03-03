use crate::actor::actor_ref::{ActorId, ActorRef};
use crate::actor::cell::ActorCell;
use crate::actor::traits::Actor;

/// The ActorSystem is the top-level container for all actors.
/// It manages the actor hierarchy, provides spawning, and owns
/// the system-level guardian actors.
pub struct ActorSystem {
    name: String,
}

impl ActorSystem {
    /// Create a new actor system with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        tracing::info!(system = %name, "actor system starting");
        Self { name }
    }

    /// Spawn a new top-level actor in this system.
    /// Returns an ActorRef for communicating with the actor.
    pub fn spawn<A: Actor>(&self, actor: A, name: impl Into<String>) -> ActorRef<A::Msg> {
        let actor_name = name.into();
        let path = format!("/{}/{}", self.name, actor_name);
        let id = ActorId::new(&actor_name, &path);

        let cell = ActorCell::new(actor, id);
        cell.spawn()
    }

    /// Get the system name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for ActorSystem {
    fn drop(&mut self) {
        tracing::info!(system = %self.name, "actor system shutting down");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::context::ActorContext;
    use crate::actor::errors::ActorError;
    use crate::actor::traits::{Actor, Message};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Notify;

    // --- Test actor: simple counter ---

    #[derive(Debug)]
    enum CounterMsg {
        Increment,
        GetCount,
    }

    impl Message for CounterMsg {}

    struct CounterActor;

    impl Actor for CounterActor {
        type Msg = CounterMsg;
        type State = i64;

        async fn pre_start(
            &self,
            _ctx: &mut ActorContext<Self>,
        ) -> Result<Self::State, ActorError> {
            Ok(0)
        }

        async fn handle(
            &self,
            msg: Self::Msg,
            state: &mut Self::State,
            ctx: &mut ActorContext<Self>,
        ) -> Result<(), ActorError> {
            match msg {
                CounterMsg::Increment => {
                    *state += 1;
                    Ok(())
                }
                CounterMsg::GetCount => {
                    ctx.reply(*state);
                    Ok(())
                }
            }
        }

        async fn post_stop(&self, _state: Self::State, _ctx: &mut ActorContext<Self>) {}
    }

    #[tokio::test]
    async fn test_spawn_and_tell() {
        let system = ActorSystem::new("test");
        let counter = system.spawn(CounterActor, "counter");

        counter.tell(CounterMsg::Increment).unwrap();
        counter.tell(CounterMsg::Increment).unwrap();
        counter.tell(CounterMsg::Increment).unwrap();

        // Give time for messages to be processed
        tokio::time::sleep(Duration::from_millis(50)).await;

        let count: i64 = counter
            .ask(CounterMsg::GetCount, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_ask_response() {
        let system = ActorSystem::new("test");
        let counter = system.spawn(CounterActor, "counter");

        let count: i64 = counter
            .ask(CounterMsg::GetCount, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 0);

        counter.tell(CounterMsg::Increment).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;

        let count: i64 = counter
            .ask(CounterMsg::GetCount, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_stop_actor() {
        let system = ActorSystem::new("test");
        let counter = system.spawn(CounterActor, "counter");

        counter.stop().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Sending to stopped actor should fail
        assert!(counter.tell(CounterMsg::Increment).is_err());
    }

    // --- Test actor: restart on failure ---

    #[derive(Debug)]
    enum FaultyMsg {
        Fail,
        Ping,
    }

    impl Message for FaultyMsg {}

    struct FaultyActor {
        started: Arc<Notify>,
    }

    impl Actor for FaultyActor {
        type Msg = FaultyMsg;
        type State = u32; // counts how many messages processed

        async fn pre_start(
            &self,
            _ctx: &mut ActorContext<Self>,
        ) -> Result<Self::State, ActorError> {
            self.started.notify_one();
            Ok(0)
        }

        async fn handle(
            &self,
            msg: Self::Msg,
            state: &mut Self::State,
            ctx: &mut ActorContext<Self>,
        ) -> Result<(), ActorError> {
            match msg {
                FaultyMsg::Fail => Err(ActorError::custom("intentional failure")),
                FaultyMsg::Ping => {
                    *state += 1;
                    ctx.reply(*state);
                    Ok(())
                }
            }
        }

        async fn post_stop(&self, _state: Self::State, _ctx: &mut ActorContext<Self>) {}
    }

    #[tokio::test]
    async fn test_restart_on_failure() {
        let started = Arc::new(Notify::new());
        let system = ActorSystem::new("test");
        let actor = system.spawn(
            FaultyActor {
                started: started.clone(),
            },
            "faulty",
        );

        // Wait for initial start
        started.notified().await;

        // Cause a failure — actor should restart
        actor.tell(FaultyMsg::Fail).unwrap();

        // Wait for restart
        started.notified().await;

        // Actor should be alive again with fresh state
        let count: u32 = actor
            .ask(FaultyMsg::Ping, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 1); // fresh state after restart
    }
}
