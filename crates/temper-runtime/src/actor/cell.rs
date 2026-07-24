use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{error, info, warn};

use super::actor_ref::{ActorId, ActorRef, Envelope, SystemSignal};
use super::context::ActorContext;
use super::errors::ActorError;
use super::traits::Actor;
use crate::mailbox::{self, DEFAULT_MAILBOX_CAPACITY, MailboxReceiver};
use crate::supervision::SupervisionStrategy;

/// The ActorCell is the runtime container for an actor instance.
/// It owns the actor, its state, its mailbox receiver, and drives the message loop.
/// This is an internal type — users interact through ActorRef only.
pub struct ActorCell<A: Actor> {
    actor: A,
    id: ActorId,
    mailbox_capacity: usize,
}

/// Clears publication readiness whenever the actor task future is dropped.
///
/// Normal shutdown, panic unwinding, and task cancellation all drop the run
/// future, so a dead incarnation can never remain externally marked ready.
struct ActorReadiness {
    lifecycle: Arc<AtomicU64>,
}

impl ActorReadiness {
    fn new(lifecycle: Arc<AtomicU64>) -> Self {
        lifecycle.store(0, Ordering::Release);
        Self { lifecycle }
    }

    fn begin_incarnation(&self) {
        let current = self.lifecycle.load(Ordering::Acquire);
        assert_eq!(
            current & 1,
            0,
            "actor cannot begin a supervised incarnation while marked ready"
        );
        let next = current
            .checked_add(2)
            .expect("actor supervised-incarnation epoch exhausted");
        self.lifecycle.store(next, Ordering::Release);
    }

    fn mark_ready(&self) {
        let previous = self.lifecycle.fetch_or(1, Ordering::AcqRel);
        assert_eq!(
            previous & 1,
            0,
            "actor supervised incarnation was already marked ready"
        );
    }

    fn mark_unready(&self) {
        self.lifecycle.fetch_and(!1, Ordering::AcqRel);
    }
}

impl Drop for ActorReadiness {
    fn drop(&mut self) {
        self.mark_unready();
    }
}

impl<A: Actor> ActorCell<A> {
    /// Create a new actor cell with the given actor and ID.
    pub fn new(actor: A, id: ActorId) -> Self {
        Self {
            actor,
            id,
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
        }
    }

    /// Set custom mailbox capacity (TigerStyle: explicit budgets).
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = capacity;
        self
    }

    /// Spawn the actor cell as a tokio task. Returns the ActorRef for external communication.
    pub fn spawn(self) -> ActorRef<A::Msg> {
        let (tx, rx) = mailbox::mailbox(self.mailbox_capacity);
        let id = self.id.clone();
        let lifecycle = Arc::new(AtomicU64::new(0));

        let actor_ref = ActorRef {
            sender: tx,
            id: id.clone(),
            lifecycle: lifecycle.clone(),
        };

        tokio::spawn(self.run(rx, lifecycle)); // determinism-ok: production actor cell, not on simulation path

        actor_ref
    }

    /// The actor's main run loop:
    /// 1. pre_start → initialize state
    /// 2. loop: receive message → handle
    /// 3. post_stop → cleanup
    async fn run(self, mut rx: MailboxReceiver<A::Msg>, lifecycle: Arc<AtomicU64>) {
        let readiness = ActorReadiness::new(lifecycle);
        let actor = self.actor;
        let id = self.id;
        let strategy = actor.supervision_strategy();

        let mut restart_count: u32 = 0;

        loop {
            readiness.mark_unready();
            readiness.begin_incarnation();
            // Phase 1: Initialize
            let mut ctx = ActorContext::new(id.clone());
            info!(actor = %id, "actor starting");

            let mut state = match actor.pre_start(&mut ctx).await {
                Ok(s) => {
                    info!(actor = %id, "actor started");
                    restart_count = 0;
                    readiness.mark_ready();
                    s
                }
                Err(e) => {
                    error!(actor = %id, error = %e, "actor pre_start failed");
                    if should_restart(&strategy, restart_count) {
                        restart_count += 1;
                        warn!(actor = %id, restart = restart_count, "restarting after init failure");
                        let backoff = strategy.backoff_duration(restart_count);
                        tokio::time::sleep(backoff).await; // determinism-ok: production actor cell, backoff between restarts
                        continue;
                    } else {
                        error!(actor = %id, "actor permanently failed during init");
                        return;
                    }
                }
            };

            // Phase 2: Message loop
            let restart_needed = 'message_loop: loop {
                let Some(envelope) = rx.recv().await else {
                    // All senders dropped — actor is orphaned, stop.
                    info!(actor = %id, "all senders dropped, stopping");
                    break 'message_loop false;
                };

                match envelope {
                    Envelope::Tell(msg) => {
                        if let Err(e) = actor.handle(msg, &mut state, &mut ctx).await {
                            error!(actor = %id, error = %e, "actor handle failed");
                            if should_restart(&strategy, restart_count) {
                                break 'message_loop true;
                            } else {
                                break 'message_loop false;
                            }
                        }
                    }
                    Envelope::Ask { msg, reply } => {
                        ctx.reply_channel = Some(reply);
                        if let Err(e) = actor.handle(msg, &mut state, &mut ctx).await {
                            error!(actor = %id, error = %e, "actor handle (ask) failed");
                            if let Some(tx) = ctx.reply_channel.take() {
                                let _ = tx
                                    .send(Err(ActorError::custom(format!("handler failed: {e}"))));
                            }
                            if should_restart(&strategy, restart_count) {
                                break 'message_loop true;
                            } else {
                                break 'message_loop false;
                            }
                        }
                        ctx.reply_channel = None;
                    }
                    Envelope::Signal(signal) => match signal {
                        SystemSignal::Stop | SystemSignal::PoisonPill => {
                            info!(actor = %id, signal = ?signal, "received stop signal");
                            break 'message_loop false;
                        }
                        SystemSignal::Restart => {
                            info!(actor = %id, "received restart signal");
                            break 'message_loop true;
                        }
                    },
                }
            };

            // Phase 3: Cleanup
            readiness.mark_unready();
            info!(actor = %id, "actor stopping");
            actor.post_stop(state, &mut ctx).await;

            if restart_needed {
                restart_count += 1;
                warn!(actor = %id, restart = restart_count, "restarting");
                let backoff = strategy.backoff_duration(restart_count);
                tokio::time::sleep(backoff).await; // determinism-ok: production actor cell, backoff between restarts
            } else {
                info!(actor = %id, "actor stopped");
                return;
            }
        }
    }
}

fn should_restart(strategy: &SupervisionStrategy, current_restarts: u32) -> bool {
    match strategy {
        SupervisionStrategy::Stop => false,
        SupervisionStrategy::Restart { max_retries, .. } => current_restarts < *max_retries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{ActorContext, Message};
    use std::time::Duration;
    use tokio::sync::Notify;

    #[derive(Debug)]
    enum PanickingMsg {
        Crash,
    }

    impl Message for PanickingMsg {}

    struct PanickingActor {
        started: Arc<Notify>,
    }

    impl Actor for PanickingActor {
        type Msg = PanickingMsg;
        type State = ();

        async fn pre_start(
            &self,
            _ctx: &mut ActorContext<Self>,
        ) -> Result<Self::State, ActorError> {
            self.started.notify_one();
            Ok(())
        }

        async fn handle(
            &self,
            msg: Self::Msg,
            _state: &mut Self::State,
            _ctx: &mut ActorContext<Self>,
        ) -> Result<(), ActorError> {
            match msg {
                PanickingMsg::Crash => panic!("intentional handler panic"),
            }
        }

        async fn post_stop(&self, _state: Self::State, _ctx: &mut ActorContext<Self>) {}
    }

    #[tokio::test]
    async fn handler_panic_clears_actor_readiness() {
        let started = Arc::new(Notify::new());
        let actor = ActorCell::new(
            PanickingActor {
                started: started.clone(),
            },
            ActorId::new("panicking", "system/panicking"),
        )
        .spawn();
        started.notified().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !actor.is_ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actor must publish readiness after pre_start");

        actor
            .tell(PanickingMsg::Crash)
            .expect("enqueue crashing message");
        tokio::time::timeout(Duration::from_secs(1), async {
            while actor.is_ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actor panic must clear readiness through the run-future drop guard");
        assert!(!actor.is_ready());
    }

    #[tokio::test]
    async fn supervised_restart_advances_ready_incarnation_without_changing_actor_id() {
        let started = Arc::new(Notify::new());
        let actor = ActorCell::new(
            PanickingActor {
                started: started.clone(),
            },
            ActorId::new("restarting", "system/restarting"),
        )
        .spawn();
        started.notified().await;
        let first_incarnation = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(incarnation) = actor.ready_incarnation() {
                    break incarnation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actor must publish its first ready incarnation");
        let actor_uid = actor.id().uid;

        actor
            .signal(SystemSignal::Restart)
            .expect("enqueue supervised restart");
        let second_incarnation = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(incarnation) = actor.ready_incarnation()
                    && incarnation != first_incarnation
                {
                    break incarnation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actor must publish its restarted incarnation");

        assert_eq!(actor.id().uid, actor_uid);
        assert!(second_incarnation > first_incarnation);
        actor.stop().expect("stop restarted actor");
    }

    #[test]
    fn stop_strategy_never_restarts() {
        let strategy = SupervisionStrategy::Stop;
        assert!(!should_restart(&strategy, 0));
        assert!(!should_restart(&strategy, 1));
        assert!(!should_restart(&strategy, 100));
    }

    #[test]
    fn restart_strategy_respects_max_retries() {
        let strategy = SupervisionStrategy::Restart {
            max_retries: 3,
            backoff_base: Duration::from_millis(100),
        };
        assert!(should_restart(&strategy, 0));
        assert!(should_restart(&strategy, 1));
        assert!(should_restart(&strategy, 2));
        assert!(!should_restart(&strategy, 3));
        assert!(!should_restart(&strategy, 4));
    }

    #[test]
    fn restart_strategy_zero_retries_never_restarts() {
        let strategy = SupervisionStrategy::Restart {
            max_retries: 0,
            backoff_base: Duration::from_millis(100),
        };
        assert!(!should_restart(&strategy, 0));
    }
}
