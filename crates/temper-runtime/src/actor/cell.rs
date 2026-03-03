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
                        tokio::time::sleep(backoff).await;
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
            info!(actor = %id, "actor stopping");
            actor.post_stop(state, &mut ctx).await;

            if restart_needed {
                restart_count += 1;
                warn!(actor = %id, restart = restart_count, "restarting");
                let backoff = strategy.backoff_duration(restart_count);
                tokio::time::sleep(backoff).await;
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
