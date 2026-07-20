//! Actor mailbox — the fundamental message queue primitive.
//!
//! Every actor has exactly one mailbox. Messages are enqueued by senders
//! (via ActorRef) and dequeued sequentially by the actor cell.
//!
//! TigerStyle: All mailboxes are BOUNDED. No unbounded queues.
//! The capacity is set at actor creation time and cannot grow.
//! When full, sends return MailboxFull immediately — no blocking, no OOM.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, mpsc};

use crate::actor::actor_ref::Envelope;
use crate::actor::errors::ActorError;
use crate::actor::traits::Message;

/// Default mailbox capacity. Sized for typical entity actors.
/// TigerStyle: This is a budget, not a suggestion.
pub const DEFAULT_MAILBOX_CAPACITY: usize = 1_000;

const MAILBOX_ACCEPTING: u8 = 0;
const MAILBOX_DRAINING_RESERVING: u8 = 1;
const MAILBOX_DRAINING_UNOWNED: u8 = 2;
const MAILBOX_DRAINING_OWNED: u8 = 3;
const MAILBOX_DRAINED: u8 = 4;

struct MailboxLifecycle {
    state: AtomicU8,
    admission_gate: Mutex<()>,
    receiver_closed: AtomicBool,
    drain_completed: Notify,
}

/// Exclusive ownership of registry-side cleanup for one mailbox drain.
///
/// Before the FIFO barrier commits, the owner holds the distinct reserving
/// state. Dropping it restores admission only if the receiver remains open.
/// Once committed, dropping ownership leaves the barrier unowned so receiver
/// shutdown or another waiter can complete cleanup without reopening admission.
pub(crate) struct MailboxDrainOwner {
    lifecycle: Arc<MailboxLifecycle>,
    barrier_committed: bool,
    active: bool,
}

enum DrainOwnership {
    Owner {
        owner: MailboxDrainOwner,
        needs_barrier: bool,
    },
    Wait,
    Complete,
}

/// The sender half of a mailbox. Held by ActorRef, cloneable.
pub struct MailboxSender<M: Message> {
    inner: mpsc::Sender<Envelope<M>>,
    capacity: usize,
    lifecycle: Arc<MailboxLifecycle>,
}

/// The receiver half of a mailbox. Held by ActorCell, not cloneable.
pub struct MailboxReceiver<M: Message> {
    inner: mpsc::Receiver<Envelope<M>>,
    lifecycle: Arc<MailboxLifecycle>,
}

/// Create a new bounded mailbox with the given capacity.
pub fn mailbox<M: Message>(capacity: usize) -> (MailboxSender<M>, MailboxReceiver<M>) {
    // TigerStyle: assert the budget is sane
    debug_assert!(capacity > 0, "mailbox capacity must be > 0");
    debug_assert!(
        capacity <= 100_000,
        "mailbox capacity {} exceeds max budget 100,000",
        capacity
    );

    let (tx, rx) = mpsc::channel(capacity);
    let lifecycle = Arc::new(MailboxLifecycle {
        state: AtomicU8::new(MAILBOX_ACCEPTING),
        admission_gate: Mutex::new(()),
        receiver_closed: AtomicBool::new(false),
        drain_completed: Notify::new(),
    });
    (
        MailboxSender {
            inner: tx,
            capacity,
            lifecycle: Arc::clone(&lifecycle),
        },
        MailboxReceiver {
            inner: rx,
            lifecycle,
        },
    )
}

impl<M: Message> MailboxSender<M> {
    /// Send a message to the mailbox. Returns MailboxFull if at capacity.
    /// TigerStyle: This never blocks. Full is an error, not a wait condition.
    pub fn send(&self, msg: Envelope<M>) -> Result<(), ActorError> {
        let _admission = self
            .lifecycle
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.lifecycle.state.load(Ordering::Acquire) != MAILBOX_ACCEPTING {
            return Err(ActorError::Stopped);
        }
        self.inner.try_send(msg).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => ActorError::MailboxFull,
            mpsc::error::TrySendError::Closed(_) => ActorError::SendFailed,
        })
    }

    /// Atomically stop admitting application traffic, then enqueue the FIFO
    /// drain barrier once bounded capacity becomes available.
    pub(crate) async fn begin_draining(&self, msg: Envelope<M>) -> Option<MailboxDrainOwner> {
        loop {
            match self.acquire_drain_ownership() {
                DrainOwnership::Owner {
                    mut owner,
                    needs_barrier,
                } => {
                    if needs_barrier {
                        match self.inner.clone().reserve_owned().await {
                            Ok(permit) => {
                                let _admission = self
                                    .lifecycle
                                    .admission_gate
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                if self.lifecycle.state.load(Ordering::Acquire)
                                    == MAILBOX_DRAINING_RESERVING
                                {
                                    permit.send(msg);
                                    self.lifecycle
                                        .state
                                        .store(MAILBOX_DRAINING_OWNED, Ordering::Release);
                                } else {
                                    drop(permit);
                                }
                                owner.barrier_committed = true;
                            }
                            Err(_) => {
                                let _admission = self
                                    .lifecycle
                                    .admission_gate
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                if self.lifecycle.state.load(Ordering::Acquire)
                                    == MAILBOX_DRAINING_RESERVING
                                {
                                    self.lifecycle
                                        .state
                                        .store(MAILBOX_DRAINED, Ordering::Release);
                                    self.lifecycle.drain_completed.notify_waiters();
                                }
                                // Receiver closure itself is the terminal FIFO
                                // barrier and must never reopen admission.
                                owner.barrier_committed = true;
                            }
                        }
                    }
                    return Some(owner);
                }
                DrainOwnership::Wait => self.wait_for_drain_owner_transition().await,
                DrainOwnership::Complete => return None,
            }
        }
    }

    fn acquire_drain_ownership(&self) -> DrainOwnership {
        let _admission = self
            .lifecycle
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match self.lifecycle.state.load(Ordering::Acquire) {
            MAILBOX_ACCEPTING => {
                self.lifecycle
                    .state
                    .store(MAILBOX_DRAINING_RESERVING, Ordering::Release);
                DrainOwnership::Owner {
                    owner: MailboxDrainOwner {
                        lifecycle: Arc::clone(&self.lifecycle),
                        barrier_committed: false,
                        active: true,
                    },
                    needs_barrier: true,
                }
            }
            MAILBOX_DRAINING_UNOWNED => {
                self.lifecycle
                    .state
                    .store(MAILBOX_DRAINING_OWNED, Ordering::Release);
                DrainOwnership::Owner {
                    owner: MailboxDrainOwner {
                        lifecycle: Arc::clone(&self.lifecycle),
                        barrier_committed: true,
                        active: true,
                    },
                    needs_barrier: false,
                }
            }
            MAILBOX_DRAINING_RESERVING | MAILBOX_DRAINING_OWNED => DrainOwnership::Wait,
            MAILBOX_DRAINED => DrainOwnership::Complete,
            unexpected => unreachable!("unknown mailbox lifecycle state {unexpected}"),
        }
    }

    async fn wait_for_drain_owner_transition(&self) {
        loop {
            if !matches!(
                self.lifecycle.state.load(Ordering::Acquire),
                MAILBOX_DRAINING_RESERVING | MAILBOX_DRAINING_OWNED
            ) {
                return;
            }
            let changed = self.lifecycle.drain_completed.notified();
            if !matches!(
                self.lifecycle.state.load(Ordering::Acquire),
                MAILBOX_DRAINING_RESERVING | MAILBOX_DRAINING_OWNED
            ) {
                return;
            }
            changed.await;
        }
    }

    /// Try to close admission and enqueue a drain barrier without waiting.
    pub(crate) fn try_begin_draining(&self, msg: Envelope<M>) -> Result<(), ActorError> {
        let _admission = self
            .lifecycle
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match self.lifecycle.state.load(Ordering::Acquire) {
            MAILBOX_ACCEPTING => {}
            MAILBOX_DRAINING_RESERVING => return Err(ActorError::MailboxFull),
            MAILBOX_DRAINING_UNOWNED | MAILBOX_DRAINING_OWNED | MAILBOX_DRAINED => {
                return Ok(());
            }
            unexpected => unreachable!("unknown mailbox lifecycle state {unexpected}"),
        }
        self.lifecycle
            .state
            .store(MAILBOX_DRAINING_UNOWNED, Ordering::Release);
        match self.inner.try_send(msg) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.lifecycle
                    .state
                    .store(MAILBOX_ACCEPTING, Ordering::Release);
                self.lifecycle.drain_completed.notify_waiters();
                Err(ActorError::MailboxFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.lifecycle
                    .state
                    .store(MAILBOX_DRAINED, Ordering::Release);
                self.lifecycle.drain_completed.notify_waiters();
                Ok(())
            }
        }
    }

    /// Wait until the actor-side mailbox receiver has closed.
    pub(crate) async fn closed(&self) {
        self.inner.closed().await;
    }

    /// Wait until the owner has completed registry-side drain cleanup.
    pub(crate) async fn wait_for_drain_completion(&self) {
        loop {
            if !self.is_draining() {
                return;
            }
            let completed = self.lifecycle.drain_completed.notified();
            if !self.is_draining() {
                return;
            }
            completed.await;
        }
    }

    /// Return whether admission is fenced pending drain cleanup.
    pub(crate) fn is_draining(&self) -> bool {
        matches!(
            self.lifecycle.state.load(Ordering::Acquire),
            MAILBOX_DRAINING_RESERVING | MAILBOX_DRAINING_UNOWNED | MAILBOX_DRAINING_OWNED
        )
    }

    /// Return whether this mailbox ever crossed its drain admission fence.
    pub(crate) fn is_drain_fenced(&self) -> bool {
        self.lifecycle.state.load(Ordering::Acquire) != MAILBOX_ACCEPTING
    }

    /// Get the mailbox capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Current number of messages queued in the mailbox.
    ///
    /// Computed as `max_capacity - available_capacity`. DST-safe:
    /// exact under single-threaded simulation.
    pub fn depth(&self) -> usize {
        self.capacity.saturating_sub(self.inner.capacity())
    }

    /// Return whether the actor-side mailbox receiver has closed.
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Mailbox utilization as a fraction in [0.0, 1.0].
    pub fn utilization(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        self.depth() as f64 / self.capacity as f64
    }
}

impl<M: Message> MailboxReceiver<M> {
    /// Receive the next message. Returns None if all senders dropped.
    pub async fn recv(&mut self) -> Option<Envelope<M>> {
        self.inner.recv().await
    }
}

impl<M: Message> Drop for MailboxReceiver<M> {
    fn drop(&mut self) {
        self.lifecycle
            .receiver_closed
            .store(true, Ordering::Release);
        let _admission = self
            .lifecycle
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(
            self.lifecycle.state.load(Ordering::Acquire),
            MAILBOX_DRAINING_RESERVING | MAILBOX_DRAINING_UNOWNED
        ) {
            self.lifecycle
                .state
                .store(MAILBOX_DRAINED, Ordering::Release);
            self.lifecycle.drain_completed.notify_waiters();
        }
    }
}

impl MailboxDrainOwner {
    /// Publish that the exclusive owner finished registry-side cleanup.
    pub(crate) fn finish(mut self) {
        let _admission = self
            .lifecycle
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.lifecycle.state.load(Ordering::Acquire) == MAILBOX_DRAINING_OWNED {
            self.lifecycle
                .state
                .store(MAILBOX_DRAINED, Ordering::Release);
        }
        self.active = false;
        self.lifecycle.drain_completed.notify_waiters();
    }
}

impl Drop for MailboxDrainOwner {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _admission = self
            .lifecycle
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = self.lifecycle.state.load(Ordering::Acquire);
        let next = match (state, self.barrier_committed) {
            (MAILBOX_DRAINING_RESERVING, false) => {
                if self.lifecycle.receiver_closed.load(Ordering::Acquire) {
                    MAILBOX_DRAINED
                } else {
                    MAILBOX_ACCEPTING
                }
            }
            (MAILBOX_DRAINING_OWNED, true) => {
                if self.lifecycle.receiver_closed.load(Ordering::Acquire) {
                    MAILBOX_DRAINED
                } else {
                    MAILBOX_DRAINING_UNOWNED
                }
            }
            _ => return,
        };
        self.lifecycle.state.store(next, Ordering::Release);
        self.lifecycle.drain_completed.notify_waiters();
    }
}

impl<M: Message> Clone for MailboxSender<M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            capacity: self.capacity,
            lifecycle: Arc::clone(&self.lifecycle),
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
