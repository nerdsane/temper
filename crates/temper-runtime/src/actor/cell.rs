use std::sync::Arc;

use tracing::{error, info, warn};

use super::actor_ref::{ActorId, ActorRef, Envelope, SystemSignal};
use super::context::ActorContext;
use super::errors::{ActorError, InitFailureKind};
use super::traits::{Actor, Message};
use crate::mailbox::{self, DEFAULT_MAILBOX_CAPACITY, MailboxReceiver};
use crate::supervision::SupervisionStrategy;

/// Notified exactly once when a cell permanently gives up on initialization.
///
/// Spawning an actor usually means recording it somewhere — a registry, an
/// index — before `pre_start` has had a chance to run. That bookkeeping has to
/// be undone when the actor never starts, and the cell is the only place that
/// knows it has stopped trying: it fires after the supervision strategy is
/// exhausted, whether or not anyone is waiting for a reply, and *before* the
/// waiting callers are answered, so a caller that sees an init failure can rely
/// on the retraction having already happened.
///
/// The observer runs on the actor's own task. It must not block for long and
/// must not send to the actor it describes.
pub type InitFailureObserver = Arc<dyn Fn(&ActorId, &ActorError) + Send + Sync>;

/// The ActorCell is the runtime container for an actor instance.
/// It owns the actor, its state, its mailbox receiver, and drives the message loop.
/// This is an internal type — users interact through ActorRef only.
pub struct ActorCell<A: Actor> {
    actor: A,
    id: ActorId,
    mailbox_capacity: usize,
    on_init_failure: Option<InitFailureObserver>,
}

impl<A: Actor> ActorCell<A> {
    /// Create a new actor cell with the given actor and ID.
    pub fn new(actor: A, id: ActorId) -> Self {
        Self {
            actor,
            id,
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
            on_init_failure: None,
        }
    }

    /// Set custom mailbox capacity (TigerStyle: explicit budgets).
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = capacity;
        self
    }

    /// Observe a permanent initialization failure — see [`InitFailureObserver`].
    pub fn with_init_failure_observer(mut self, observer: InitFailureObserver) -> Self {
        self.on_init_failure = Some(observer);
        self
    }

    /// Spawn the actor cell as a tokio task. Returns the ActorRef for external communication.
    pub fn spawn(self) -> ActorRef<A::Msg> {
        let (tx, rx) = mailbox::mailbox(self.mailbox_capacity);
        let id = self.id.clone();

        let actor_ref = ActorRef {
            sender: tx,
            id: id.clone(),
        };

        tokio::spawn(self.run(rx)); // determinism-ok: production actor cell, not on simulation path

        actor_ref
    }

    /// The actor's main run loop:
    /// 1. pre_start → initialize state
    /// 2. loop: receive message → handle
    /// 3. post_stop → cleanup
    async fn run(self, mut rx: MailboxReceiver<A::Msg>) {
        let actor = self.actor;
        let id = self.id;
        let on_init_failure = self.on_init_failure;
        let strategy = actor.supervision_strategy();

        let mut restart_count: u32 = 0;

        loop {
            // Phase 1: Initialize
            let mut ctx = ActorContext::new(id.clone());
            info!(actor = %id, "actor starting");

            let mut state = match actor.pre_start(&mut ctx).await {
                Ok(s) => {
                    info!(actor = %id, "actor started");
                    restart_count = 0;
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
                        // The cell is giving up. Tell the observer first: the
                        // spawner's bookkeeping has to be retracted even when
                        // the mailbox is empty (nobody ever asked, or the only
                        // caller walked away), and a caller that is about to be
                        // handed this failure must find the retraction already
                        // done rather than race it.
                        if let Some(observer) = on_init_failure.as_ref() {
                            observer(&id, &e);
                        }
                        // Everything already queued is owed an answer: dropping
                        // the reply channels turns every waiting `ask` into a
                        // bare `Stopped` ("actor stopped") and throws the real
                        // cause away, which is how an init failure became an
                        // unexplained 500.
                        let (asks, tells) = fail_pending(&mut rx, &e);
                        error!(
                            actor = %id,
                            error = %e,
                            failed_asks = asks,
                            dropped_tells = tells,
                            "actor permanently failed during init"
                        );
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

/// Close the mailbox and answer everything still queued with the init failure
/// that killed the actor.
///
/// Returns `(asks_failed, tells_dropped)`. Closing first means a sender racing
/// this shutdown gets `SendFailed` instead of queueing behind us forever.
fn fail_pending<M: Message>(rx: &mut MailboxReceiver<M>, cause: &ActorError) -> (usize, usize) {
    rx.close();

    let mut asks = 0usize;
    let mut tells = 0usize;
    while let Some(envelope) = rx.try_recv() {
        match envelope {
            Envelope::Ask { reply, .. } => {
                // One error per waiting caller: ActorError is not Clone
                // (anyhow::Error is not), so re-derive it from the cause.
                let _ = reply.send(Err(init_failure_for(cause)));
                asks += 1;
            }
            Envelope::Tell(_) | Envelope::Signal(_) => tells += 1,
        }
    }
    (asks, tells)
}

/// Restate a `pre_start` error as the init failure the caller should see.
///
/// An actor that classified its own failure (see [`ActorError::init_failed`])
/// keeps that classification; anything else is preserved verbatim as the cause
/// and inherits the transience the error already declares.
fn init_failure_for(cause: &ActorError) -> ActorError {
    match cause {
        ActorError::InitFailed { cause, kind } => ActorError::init_failed(cause.clone(), *kind),
        other => ActorError::init_failed(
            other.to_string(),
            if other.is_transient() {
                InitFailureKind::TransientDependency
            } else {
                InitFailureKind::Defect
            },
        ),
    }
}

#[cfg(test)]
#[path = "cell_test.rs"]
mod tests;
