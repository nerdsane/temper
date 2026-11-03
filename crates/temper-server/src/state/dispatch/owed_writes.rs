//! Writes a committed move owes, kept in memory until the store takes them.
//!
//! A move that commits can owe further writes, each a move of its own sent
//! after the commit: its module's or adapter's answer, the action its state
//! timer fires, a reaction on another record, a child it spawns. The store
//! failing one is not final: the write is kept here and tried again, first
//! after [`FIRST_RETRY_WAIT`] and then twice as long after each try, up to
//! [`LAST_RETRY_WAIT`], until it lands or fails for another reason. Any
//! other failure (Cedar, a guard or the record's state refusing it, an actor
//! whose mailbox is full) is final, and each owed write handles it as it did
//! before. A scheduled action's fire waits the same way. A state timer keeps
//! its own retry loop (`state_timer_fire`), with the dispatch retry wait and
//! its gate on every attempt; its retained write is tracked here too.
//!
//! Every try carries the same idempotency key, derived from the key the move
//! committed under and the owed write's identity (a timer's fire, from its
//! arming and the task that fires it). An unkeyed caller's move
//! gets a made-up key when it is dispatched, so its follow-ups derive theirs
//! from that one. A try whose write landed though its answer was lost (the
//! outcome unknown) is then answered as a duplicate by the next try, and the
//! write applies once.
//!
//! Which failures are the store's is read from the reply ([`Attempt::of`]):
//! a reply marked `store_unavailable`, which the actor answers for a
//! `Storage`, `OutcomeUnknown` or `StorageFull` persistence error, or
//! [`DispatchError::StoreUnavailable`], which carries one. An actor that did
//! not answer, because the ask timed out or the actor stopped first, is
//! treated the same: the write may yet land, or was never handled.
//!
//! Kept writes live only in this process, so a restart loses them, except a
//! state timer's fire, whose row arms it again at boot, and the answer of a
//! background module call that names an `on_failure`, whose deadline row
//! answers the call with it after the restart (`request_context::calls`). The gauge
//! `temper_owed_writes_held` says how many are held now, and
//! [`ServerState::report_owed_writes_at_shutdown`] logs and counts each one
//! still held when the process stops.

use std::future::Future;
use std::pin::Pin;
use std::sync::PoisonError;
use std::time::Duration;

use temper_runtime::actor::ActorError;
use temper_runtime::tenant::TenantId;
use tracing::Instrument;

use super::{DispatchError, DispatchExtOptions, WasmEntityRef};
use crate::entity_actor::EntityResponse;
use crate::idempotency::IdempotencyKey;
use crate::request_context::AgentContext;
use crate::runtime_metrics::owed_writes as metrics;
use crate::state::ServerState;

/// The wait before a write that did not commit is tried again; it doubles
/// with each try, up to [`LAST_RETRY_WAIT`].
pub(super) const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The longest wait between tries: a store that is down for long is tried
/// about once an hour, not hammered.
pub(super) const LAST_RETRY_WAIT: Duration = Duration::from_secs(3600);

/// Which write a committed move owes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwedKind {
    /// A module's or adapter's answer: its trigger's callback action.
    Answer,
    /// The action a state timer fires.
    Timer,
    /// A reaction's move on its target.
    Reaction,
    /// A spawned child: its creation and its initial action.
    Child,
}

impl OwedKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Timer => "timer",
            Self::Reaction => "reaction",
            Self::Child => "child",
        }
    }

    /// What a restart does to a write of this kind still held.
    fn at_restart(self) -> &'static str {
        match self {
            Self::Timer => "its row fires it again at boot",
            Self::Answer => {
                "a restart loses it; a module call that names an on_failure is answered by it after the restart"
            }
            Self::Reaction | Self::Child => "a restart loses it",
        }
    }
}

/// An owed write: the record it writes, which write it is, and its key.
#[derive(Debug, Clone)]
pub(crate) struct OwedWrite {
    pub(crate) tenant: TenantId,
    pub(crate) entity_type: String,
    pub(crate) entity_id: String,
    pub(crate) kind: OwedKind,
    /// The key every try carries. A write without one is not kept: a try
    /// whose write landed unseen would apply again.
    pub(crate) key: Option<IdempotencyKey>,
}

/// What one try of an owed write did.
#[derive(Debug)]
pub(crate) enum Attempt {
    /// It committed, or already had under its key, or is no longer owed.
    Landed,
    /// It did not land for a reason other than the store's: final.
    Failed(String),
    /// The store failed it, or its record's actor did not answer: it was not
    /// written, or its outcome is unknown.
    StoreFailed(String),
}

impl Attempt {
    /// What a dispatch's `reply` says about its write.
    pub(crate) fn of(reply: &Result<EntityResponse, DispatchError>) -> Self {
        match reply {
            Ok(response) if response.success => Self::Landed,
            Ok(response) => {
                let error = response
                    .error
                    .clone()
                    .unwrap_or_else(|| "write failed".into());
                if response.store_unavailable {
                    Self::StoreFailed(error)
                } else {
                    Self::Failed(error)
                }
            }
            Err(error) => Self::of_error(error),
        }
    }

    /// What a dispatch that failed with `error`, or a read of the record it
    /// writes, says about its write.
    ///
    /// An ask the actor did not answer in time is still in its mailbox and
    /// may land later, so the write is kept whatever kept the actor: the ask
    /// carries no reason, and a later try that meets the write landed is
    /// answered as a duplicate under the same key. An actor that stopped
    /// before answering (its starts failed, as they do while its journal
    /// cannot be read) never handled the move, and a later try starts it
    /// again. A write kept for a reason that never clears stays held, which
    /// `temper_owed_writes_held` shows.
    pub(crate) fn of_error(error: &DispatchError) -> Self {
        match error {
            DispatchError::StoreUnavailable(_) => Self::StoreFailed(error.to_string()),
            DispatchError::Transient {
                source: ActorError::AskTimeout(_),
                ..
            } => Self::StoreFailed(error.to_string()),
            DispatchError::Permanent {
                source: ActorError::Stopped,
            } => Self::StoreFailed(error.to_string()),
            _ => Self::Failed(error.to_string()),
        }
    }
}

/// One more try of an owed write.
pub(crate) type OwedTry = Pin<Box<dyn Future<Output = Attempt> + Send>>;

/// A move owed as it was first sent: its action, parameters and context,
/// key included, so sending it again sends the same move.
#[derive(Clone)]
pub(crate) struct OwedMove {
    pub(crate) tenant: TenantId,
    pub(crate) entity_type: String,
    pub(crate) entity_id: String,
    pub(crate) action: String,
    pub(crate) params: serde_json::Value,
    pub(crate) agent_ctx: AgentContext,
}

impl OwedMove {
    /// Send the move, its integrations in the background.
    pub(crate) async fn send(&self, state: &ServerState) -> Result<EntityResponse, DispatchError> {
        let options = DispatchExtOptions {
            agent_ctx: &self.agent_ctx,
            await_integration: false,
            await_reactions: true,
        };
        state
            .dispatch_tenant_action_ext_typed(
                &self.tenant,
                &self.entity_type,
                &self.entity_id,
                &self.action,
                self.params.clone(),
                options,
            )
            .await
    }

    /// The owed write this move is.
    pub(crate) fn owed(&self, kind: OwedKind) -> OwedWrite {
        OwedWrite {
            tenant: self.tenant.clone(),
            entity_type: self.entity_type.clone(),
            entity_id: self.entity_id.clone(),
            kind,
            key: self.agent_ctx.idempotency_key.clone(),
        }
    }

    pub(super) fn entity_ref(&self) -> WasmEntityRef<'_> {
        WasmEntityRef {
            tenant: &self.tenant,
            entity_type: &self.entity_type,
            entity_id: &self.entity_id,
        }
    }
}

impl ServerState {
    /// Keep `owed`, whose try the store failed with `error`, and run
    /// `try_again` with the waits above until a try lands or fails for
    /// another reason. A write already held under its key is left to the task
    /// that holds it.
    pub(crate) fn keep_owed_write<F>(&self, owed: OwedWrite, error: &str, try_again: F)
    where
        F: Fn(ServerState) -> OwedTry + Send + 'static,
    {
        let Some(key) = owed.key.as_ref().map(|key| key.as_str().to_string()) else {
            tracing::error!(tenant = %owed.tenant, entity_type = %owed.entity_type,
                entity_id = %owed.entity_id, owed_write = owed.kind.as_str(), %error,
                "owed write not kept: without an idempotency key a try could apply it twice");
            return;
        };
        if !self.track_owed_write(&key, owed.clone()) {
            return;
        }
        tracing::warn!(tenant = %owed.tenant, entity_type = %owed.entity_type,
            entity_id = %owed.entity_id, owed_write = owed.kind.as_str(), idempotency_key = %key,
            %error, retry_in_ms = FIRST_RETRY_WAIT.as_millis() as u64,
            "owed write did not land; keeping it to try again");
        let span = crate::trace_context::runtime_work_span!(
            crate::trace_context::cause(),
            kind = "owed_write",
            "dispatch.owed_write",
            tenant = %owed.tenant,
            entity_type = %owed.entity_type,
            entity_id = %owed.entity_id,
            owed_write = owed.kind.as_str(),
        );
        let state = self.clone();
        let retries = async move {
            let mut wait = FIRST_RETRY_WAIT;
            loop {
                tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                wait = (wait * 2).min(LAST_RETRY_WAIT);
                match try_again(state.clone()).await {
                    Attempt::Landed => break,
                    Attempt::Failed(error) => {
                        tracing::warn!(tenant = %owed.tenant, entity_type = %owed.entity_type,
                            entity_id = %owed.entity_id, owed_write = owed.kind.as_str(),
                            idempotency_key = %key, %error, "owed write failed when tried again");
                        break;
                    }
                    Attempt::StoreFailed(error) => {
                        tracing::warn!(tenant = %owed.tenant, entity_type = %owed.entity_type,
                            entity_id = %owed.entity_id, owed_write = owed.kind.as_str(),
                            idempotency_key = %key, %error, retry_in_ms = wait.as_millis() as u64,
                            "owed write did not land; trying it again");
                    }
                }
            }
            state.untrack_owed_write(&key);
        };
        tokio::spawn(retries.instrument(span)); // determinism-ok: an owed write's retries are a background side-effect
    }

    /// Track a retained write, including one whose timer task owns its retries.
    pub(super) fn track_owed_write(&self, key: &str, owed: OwedWrite) -> bool {
        let mut held = self
            .owed_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if held.contains_key(key) {
            return false;
        }
        held.insert(key.to_string(), owed);
        metrics::record_held(held.len());
        true
    }

    /// Whether a write is held under `key`, still to be tried again.
    pub(super) fn holds_owed_write(&self, key: &str) -> bool {
        self.owed_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(key)
    }

    /// Stop tracking a retained write once it lands or ends.
    pub(super) fn untrack_owed_write(&self, key: &str) {
        let mut held = self
            .owed_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        held.remove(key);
        metrics::record_held(held.len());
    }

    /// Log and count each owed write still held as the process stops, so a
    /// write a restart drops is seen rather than lost silently.
    pub fn report_owed_writes_at_shutdown(&self) {
        let held = self
            .owed_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (key, owed) in held.iter() {
            tracing::error!(tenant = %owed.tenant, entity_type = %owed.entity_type,
                entity_id = %owed.entity_id, owed_write = owed.kind.as_str(),
                idempotency_key = %key, "owed write still held as the process stops: {}",
                owed.kind.at_restart());
            metrics::record_held_at_shutdown(
                owed.tenant.as_str(),
                &owed.entity_type,
                owed.kind.as_str(),
            );
        }
    }
}

#[cfg(test)]
#[path = "owed_writes_test.rs"]
mod tests;
