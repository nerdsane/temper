use tracing::{error, info, warn};

use super::actor_ref::{ActorId, ActorRef, Envelope, SystemSignal};
use super::context::ActorContext;
use super::errors::ActorError;
use super::traits::Actor;
use crate::mailbox::{self, DEFAULT_MAILBOX_CAPACITY, MailboxReceiver};
use crate::supervision::SupervisionStrategy;
use tracing::Instrument;

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

        // The first start loads the actor's state for whoever woke it, so it
        // runs inside the spawner's span rather than starting its own trace.
        let woken_by = tracing::Span::current();
        tokio::spawn(self.run(rx, woken_by)); // determinism-ok: production actor cell, not on simulation path

        actor_ref
    }

    /// The actor's main run loop:
    /// 1. pre_start → initialize state
    /// 2. loop: receive message → handle
    /// 3. post_stop → cleanup
    async fn run(self, mut rx: MailboxReceiver<A::Msg>, woken_by: tracing::Span) {
        let actor = self.actor;
        let id = self.id;
        let strategy = actor.supervision_strategy();

        let mut restart_count: u32 = 0;
        let mut woken_by = Some(woken_by);

        loop {
            // Phase 1: Initialize
            let mut ctx = ActorContext::new(id.clone());
            info!(actor = %id, "actor starting");

            let start_span = woken_by.take().unwrap_or_else(tracing::Span::none);
            let mut state = match actor.pre_start(&mut ctx).instrument(start_span).await {
                Ok(s) => {
                    info!(actor = %id, "actor started");
                    restart_count = 0;
                    s
                }
                // The actor refused to start for a reason a restart here does
                // not fix: it stops now, and every ask waiting on it is told why
                // instead of finding it stopped.
                Err(ActorError::InitFailed(reason)) => {
                    warn!(actor = %id, reason = %reason, "actor did not start");
                    rx.close_not_started(&reason);
                    while let Some(envelope) = rx.recv().await {
                        if let Envelope::Ask { reply, .. } = envelope {
                            let _ = reply.send(Err(ActorError::InitFailed(reason.clone())));
                        }
                    }
                    return;
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
            let mut abandoned_asks: usize = 0;
            let restart_needed = 'message_loop: loop {
                let Some(envelope) = rx.recv().await else {
                    // All senders dropped — actor is orphaned, stop.
                    info!(actor = %id, "all senders dropped, stopping");
                    break 'message_loop false;
                };

                // An ask whose caller has gone is dropped without running: nobody
                // can learn its answer, and behind a slow message such asks are
                // what the queue is made of. Running them would only delay the
                // callers still waiting.
                if let Envelope::Ask { reply, .. } = &envelope
                    && reply.is_closed()
                {
                    abandoned_asks += 1;
                    if rx.is_empty() {
                        report_abandoned_asks(&id, &mut abandoned_asks);
                    }
                    continue;
                }
                report_abandoned_asks(&id, &mut abandoned_asks);

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
                    Envelope::Ask { msg, reply, span } => {
                        ctx.reply_channel = Some(reply);
                        if let Err(e) = actor
                            .handle(msg, &mut state, &mut ctx)
                            .instrument(span)
                            .await
                        {
                            error!(actor = %id, error = %e, "actor handle (ask) failed");
                            if let Some(tx) = ctx.reply_channel.take() {
                                // Stopped keeps its kind: it tells the caller the
                                // message did not finish and can be tried again.
                                let answer = match e {
                                    ActorError::Stopped => ActorError::Stopped,
                                    e => ActorError::custom(format!("handler failed: {e}")),
                                };
                                let _ = tx.send(Err(answer));
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

/// Log how many asks were dropped since the last line: one line per run of them,
/// so a long queue of abandoned asks costs one line.
fn report_abandoned_asks(id: &ActorId, abandoned_asks: &mut usize) {
    if *abandoned_asks > 0 {
        warn!(actor = %id, dropped = *abandoned_asks, "dropped asks whose caller has gone");
        *abandoned_asks = 0;
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
    use crate::actor::traits::Message;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[derive(Debug)]
    enum Step {
        /// Hold the actor until the sender goes.
        Hold(oneshot::Receiver<()>),
        /// Count this message and answer it.
        Count,
    }
    impl Message for Step {}

    struct Counting(Arc<AtomicUsize>);

    impl Actor for Counting {
        type Msg = Step;
        type State = ();

        async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<(), ActorError> {
            Ok(())
        }

        async fn handle(
            &self,
            msg: Step,
            _state: &mut (),
            ctx: &mut ActorContext<Self>,
        ) -> Result<(), ActorError> {
            match msg {
                Step::Hold(until) => {
                    let _ = until.await;
                }
                Step::Count => {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    ctx.reply(());
                }
            }
            Ok(())
        }

        async fn post_stop(&self, _state: (), _ctx: &mut ActorContext<Self>) {}
    }

    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<u8>>>);

    impl Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn an_ask_whose_caller_has_gone_never_runs_and_the_line_counts_them() {
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _logs = tracing::subscriber::set_default(subscriber);

        let counted = Arc::new(AtomicUsize::new(0));
        let actor = ActorCell::new(
            Counting(counted.clone()),
            ActorId::new("count", "test/count"),
        )
        .spawn();
        let (release, until) = oneshot::channel();
        actor.tell(Step::Hold(until)).unwrap();

        // Two callers give up behind the held message; one keeps waiting.
        for _ in 0..2 {
            let gave_up = actor.ask::<()>(Step::Count, Duration::from_millis(5)).await;
            assert_eq!(gave_up, Err(ActorError::AskTimeout(Duration::ZERO)));
        }
        let waiting = actor.clone();
        let live =
            tokio::spawn(
                async move { waiting.ask::<()>(Step::Count, Duration::from_secs(5)).await },
            );
        tokio::task::yield_now().await;
        assert_eq!(counted.load(Ordering::SeqCst), 0);

        drop(release);
        live.await.unwrap().unwrap();
        assert_eq!(
            counted.load(Ordering::SeqCst),
            1,
            "only the caller still waiting runs"
        );
        let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            text.matches("dropped asks whose caller has gone").count(),
            1,
            "{text}"
        );
        assert!(text.contains("dropped=2"), "{text}");
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
