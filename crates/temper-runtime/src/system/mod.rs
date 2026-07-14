use crate::actor::actor_ref::{ActorId, ActorRef, PendingAsk};
use crate::actor::cell::ActorCell;
use crate::actor::errors::ActorError;
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

    /// Spawn an actor with one ask guaranteed to be first in its mailbox.
    ///
    /// The ask is synchronously admitted before the new [`ActorRef`] escapes
    /// this method. Callers may enqueue later traffic while `pre_start` is
    /// pending, but FIFO mailbox order guarantees that the actor handles this
    /// first ask before that traffic.
    pub fn spawn_with_first_ask<A: Actor, R: Send + 'static>(
        &self,
        actor: A,
        name: impl Into<String>,
        first_msg: A::Msg,
    ) -> Result<(ActorRef<A::Msg>, PendingAsk<R>), ActorError> {
        let actor_ref = self.spawn(actor, name);
        let pending = actor_ref.enqueue_ask(first_msg)?;
        Ok((actor_ref, pending))
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

    #[derive(Debug)]
    struct LifecycleMsg(&'static str);

    impl Message for LifecycleMsg {}

    enum StartupBehavior {
        Wait(Arc<Notify>),
        Fail,
    }

    struct LifecycleActor {
        startup: StartupBehavior,
    }

    impl Actor for LifecycleActor {
        type Msg = LifecycleMsg;
        type State = Vec<String>;

        fn supervision_strategy(&self) -> crate::supervision::SupervisionStrategy {
            crate::supervision::SupervisionStrategy::Stop
        }

        async fn pre_start(
            &self,
            _ctx: &mut ActorContext<Self>,
        ) -> Result<Self::State, ActorError> {
            match &self.startup {
                StartupBehavior::Wait(gate) => {
                    gate.notified().await;
                    Ok(Vec::new())
                }
                StartupBehavior::Fail => {
                    Err(ActorError::InitFailed("expected test failure".to_string()))
                }
            }
        }

        async fn handle(
            &self,
            msg: Self::Msg,
            state: &mut Self::State,
            ctx: &mut ActorContext<Self>,
        ) -> Result<(), ActorError> {
            state.push(msg.0.to_string());
            ctx.reply(state.clone());
            Ok(())
        }

        async fn post_stop(&self, _state: Self::State, _ctx: &mut ActorContext<Self>) {}
    }

    #[tokio::test]
    async fn first_ask_precedes_messages_queued_after_spawn() {
        let gate = Arc::new(Notify::new());
        let system = ActorSystem::new("test");
        let (actor, first) = system
            .spawn_with_first_ask::<_, Vec<String>>(
                LifecycleActor {
                    startup: StartupBehavior::Wait(gate.clone()),
                },
                "delayed",
                LifecycleMsg("first"),
            )
            .expect("the first ask fits in a fresh actor mailbox");
        actor
            .tell(LifecycleMsg("queued"))
            .expect("the follow-up message fits in the mailbox");

        let first_reply = first.receive();
        tokio::pin!(first_reply);
        tokio::select! {
            biased;
            result = &mut first_reply => panic!("first ask completed before pre_start: {result:?}"),
            () = tokio::task::yield_now() => {}
        }

        gate.notify_one();
        let first_state = first_reply
            .await
            .expect("first ask must complete after pre_start succeeds");
        assert_eq!(first_state, vec!["first"]);

        let final_state = actor
            .ask::<Vec<String>>(LifecycleMsg("probe"), Duration::from_secs(1))
            .await
            .expect("the ready actor must process messages");
        assert_eq!(final_state, vec!["first", "queued", "probe"]);
    }

    #[tokio::test]
    async fn first_ask_reports_permanent_start_failure() {
        let system = ActorSystem::new("test");
        let (actor, first) = system
            .spawn_with_first_ask::<_, Vec<String>>(
                LifecycleActor {
                    startup: StartupBehavior::Fail,
                },
                "failed-start",
                LifecycleMsg("first"),
            )
            .expect("the first ask is admitted before startup runs");

        assert_eq!(first.receive().await, Err(ActorError::Stopped));
        assert!(
            actor.is_stopped(),
            "a permanently failed startup must close the mailbox incarnation"
        );
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
