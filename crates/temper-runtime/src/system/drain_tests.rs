use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use super::ActorSystem;
use crate::actor::context::ActorContext;
use crate::actor::errors::ActorError;
use crate::actor::traits::{Actor, Message};

#[derive(Debug)]
struct DrainMsg;

impl Message for DrainMsg {}

struct BlockingActor {
    started: Arc<Notify>,
    release: Arc<Notify>,
    stopped: Arc<Notify>,
}

impl Actor for BlockingActor {
    type Msg = DrainMsg;
    type State = ();

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
        Ok(())
    }

    async fn handle(
        &self,
        _msg: Self::Msg,
        _state: &mut Self::State,
        _ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(())
    }

    async fn post_stop(&self, _state: Self::State, _ctx: &mut ActorContext<Self>) {
        self.stopped.notify_one();
    }
}

struct IdleActor;

impl Actor for IdleActor {
    type Msg = DrainMsg;
    type State = ();

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
        Ok(())
    }

    async fn handle(
        &self,
        _msg: Self::Msg,
        _state: &mut Self::State,
        _ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        Ok(())
    }

    async fn post_stop(&self, _state: Self::State, _ctx: &mut ActorContext<Self>) {}
}

fn spawn_blocking(
    system: &ActorSystem,
    name: &str,
) -> (
    crate::actor::ActorRef<DrainMsg>,
    Arc<Notify>,
    Arc<Notify>,
    Arc<Notify>,
) {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let stopped = Arc::new(Notify::new());
    let actor = system.spawn(
        BlockingActor {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            stopped: Arc::clone(&stopped),
        },
        name,
    );
    (actor, started, release, stopped)
}

#[tokio::test]
async fn stop_and_wait_does_not_finish_before_an_inflight_handler() {
    let system = ActorSystem::new("test");
    let (actor, started, release, stopped) = spawn_blocking(&system, "blocking");
    actor
        .tell(DrainMsg)
        .expect("the blocking message fits in a fresh mailbox");
    started.notified().await;

    let stop = actor.stop_and_wait();
    tokio::pin!(stop);
    tokio::select! {
        biased;
        result = &mut stop => panic!("stop completed before the handler: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    for _ in 0..32 {
        if actor.mailbox_depth() == 1 {
            break;
        }
        tokio::select! {
            biased;
            result = &mut stop => panic!("stop completed before the handler: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(
        actor.mailbox_depth(),
        1,
        "the FIFO stop barrier must be admitted before probing late traffic"
    );
    assert_eq!(
        actor.tell(DrainMsg),
        Err(ActorError::Stopped),
        "traffic admitted after Stop would be dropped when the actor exits"
    );

    release.notify_one();
    let _drain_guard = stop.await.expect("the actor stops after its handler exits");
    stopped.notified().await;
    assert!(actor.is_stopped());
}

#[tokio::test]
async fn cancelled_stop_wait_completes_after_the_actor_exits() {
    let system = ActorSystem::new("test");
    let (actor, started, release, stopped) = spawn_blocking(&system, "cancelled-stop-wait");
    actor
        .tell(DrainMsg)
        .expect("the blocking message fits in a fresh mailbox");
    started.notified().await;

    let mut stop = Box::pin(actor.stop_and_wait());
    for _ in 0..32 {
        if actor.mailbox_depth() == 1 {
            break;
        }
        tokio::select! {
            biased;
            result = &mut stop => panic!("stop completed before the handler: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(actor.mailbox_depth(), 1, "the Stop barrier is queued");
    drop(stop);

    release.notify_one();
    stopped.notified().await;
    tokio::time::timeout(Duration::from_secs(1), actor.wait_for_drain_completion())
        .await
        .expect("cancelling the waiter must not strand the drain lifecycle");
    assert!(actor.is_stopped());
    assert!(!actor.is_draining());
}

#[tokio::test]
async fn concurrent_stop_waiters_do_not_share_cleanup_ownership() {
    let system = ActorSystem::new("test");
    let (actor, started, release, stopped) = spawn_blocking(&system, "exclusive-stop-owner");
    actor
        .tell(DrainMsg)
        .expect("the blocking message fits in a fresh mailbox");
    started.notified().await;

    let mut owner = Box::pin(actor.stop_and_wait());
    for _ in 0..32 {
        if actor.mailbox_depth() == 1 {
            break;
        }
        tokio::select! {
            biased;
            result = &mut owner => panic!("owner completed before the handler: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    let mut observer = Box::pin(actor.stop_and_wait());
    tokio::select! {
        biased;
        result = &mut observer => panic!("observer completed before the actor: {result:?}"),
        () = tokio::task::yield_now() => {}
    }

    release.notify_one();
    let owner_guard = owner.await.expect("the drain owner observes actor exit");
    stopped.notified().await;
    tokio::select! {
        biased;
        result = &mut observer => panic!("observer bypassed the owner's cleanup guard: {result:?}"),
        () = tokio::task::yield_now() => {}
    }

    drop(owner_guard);
    let _observer_guard = tokio::time::timeout(Duration::from_secs(1), observer)
        .await
        .expect("the observer is released after owner cleanup")
        .expect("the completed drain remains successful");
}

#[tokio::test]
async fn fire_and_forget_stop_releases_drain_waiters_on_exit() {
    let system = ActorSystem::new("test");
    let actor = system.spawn(IdleActor, "fire-and-forget-stop");

    actor
        .stop()
        .expect("the Stop signal fits in an empty mailbox");
    tokio::time::timeout(Duration::from_secs(1), actor.wait_for_drain_completion())
        .await
        .expect("receiver exit completes an unowned drain");

    assert!(actor.is_stopped());
    assert!(!actor.is_draining());
}
