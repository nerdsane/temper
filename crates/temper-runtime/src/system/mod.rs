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
    use std::sync::atomic::{AtomicU32, Ordering};
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

    // --- A start refused with InitFailed is not restarted ---

    struct RefusingActor {
        starts: Arc<AtomicU32>,
    }

    impl Actor for RefusingActor {
        type Msg = FaultyMsg;
        type State = u32;

        async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<u32, ActorError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Err(ActorError::InitFailed(
                "its first write was not taken".into(),
            ))
        }

        async fn handle(
            &self,
            _msg: FaultyMsg,
            _state: &mut u32,
            _ctx: &mut ActorContext<Self>,
        ) -> Result<(), ActorError> {
            Ok(())
        }

        async fn post_stop(&self, _state: u32, _ctx: &mut ActorContext<Self>) {}
    }

    #[tokio::test]
    async fn a_refused_start_tells_the_asker_why_and_is_not_restarted() {
        let starts = Arc::new(AtomicU32::new(0));
        let system = ActorSystem::new("test");
        let actor = system.spawn(
            RefusingActor {
                starts: starts.clone(),
            },
            "refusing",
        );
        match actor
            .ask::<u32>(FaultyMsg::Ping, Duration::from_secs(5))
            .await
        {
            Err(ActorError::InitFailed(reason)) => {
                assert_eq!(reason, "its first write was not taken");
            }
            other => panic!("the asker is told why the actor did not start, got {other:?}"),
        }
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "a refused start is not restarted"
        );
        assert!(
            actor.is_closed(),
            "an actor that did not start takes no messages"
        );
        // On a multi-threaded runtime the start can be refused before the
        // creator's first ask is sent; that ask is told why all the same.
        match actor
            .ask::<u32>(FaultyMsg::Ping, Duration::from_secs(5))
            .await
        {
            Err(ActorError::InitFailed(reason)) => {
                assert_eq!(reason, "its first write was not taken");
            }
            other => panic!("an ask sent after the refusal is told why, got {other:?}"),
        }
    }

    // --- An ask is handled inside the asker's span ---

    #[derive(Debug)]
    struct WhichSpan;

    impl Message for WhichSpan {}

    struct SpanReporter;

    impl Actor for SpanReporter {
        type Msg = WhichSpan;
        type State = ();

        async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<(), ActorError> {
            Ok(())
        }

        async fn handle(
            &self,
            _msg: WhichSpan,
            _state: &mut (),
            ctx: &mut ActorContext<Self>,
        ) -> Result<(), ActorError> {
            ctx.reply(tracing::Span::current().id());
            Ok(())
        }

        async fn post_stop(&self, _state: (), _ctx: &mut ActorContext<Self>) {}
    }

    #[tokio::test]
    async fn an_ask_is_handled_inside_the_askers_span() {
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry());
        let system = ActorSystem::new("test");
        let actor = system.spawn(SpanReporter, "span-reporter");
        let asker = tracing::info_span!("asker");
        let handled_in: Option<tracing::Id> = {
            let ask = actor.ask(WhichSpan, Duration::from_secs(1));
            tracing::Instrument::instrument(ask, asker.clone())
                .await
                .unwrap()
        };
        assert!(asker.id().is_some());
        assert_eq!(handled_in, asker.id());
    }

    // --- An actor's first start runs inside the span that woke it ---

    struct StartReporter {
        started_in: Arc<std::sync::Mutex<Option<tracing::Id>>>,
        started: Arc<Notify>,
    }

    impl Actor for StartReporter {
        type Msg = WhichSpan;
        type State = ();

        async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<(), ActorError> {
            *self.started_in.lock().unwrap() = tracing::Span::current().id();
            self.started.notify_one();
            Ok(())
        }

        async fn handle(
            &self,
            _msg: WhichSpan,
            _state: &mut (),
            _ctx: &mut ActorContext<Self>,
        ) -> Result<(), ActorError> {
            Ok(())
        }

        async fn post_stop(&self, _state: (), _ctx: &mut ActorContext<Self>) {}
    }

    #[tokio::test]
    async fn an_actor_starts_inside_the_span_that_woke_it() {
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry());
        let system = ActorSystem::new("test");
        let started_in = Arc::new(std::sync::Mutex::new(None));
        let started = Arc::new(Notify::new());
        let waker = tracing::info_span!("waker");
        let _actor = waker.in_scope(|| {
            system.spawn(
                StartReporter {
                    started_in: started_in.clone(),
                    started: started.clone(),
                },
                "start-reporter",
            )
        });
        started.notified().await;
        assert!(waker.id().is_some());
        assert_eq!(*started_in.lock().unwrap(), waker.id());
    }
}
