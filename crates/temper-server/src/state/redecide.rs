//! A write is decided on the record as it is when the write lands.
//!
//! A write's checks (Cedar, the relation and invariant checks, the account
//! checks) read the record, and the record's actor then commits only if the
//! record is still the state they read: the authorization precondition, a
//! digest of that state. When another write lands in between, the actor
//! refuses without writing, with `Refusal::RecordChanged`
//! (`EntityResponse::is_record_changed`). Only the actor's own checks give
//! that reason, so an answer from after a commit (an integration callback's,
//! say) is never mistaken for it. Routes using this helper read the record again
//! and run every check again on it, instead of answering 409 for a write its
//! policy allows. Nothing is decided on old state, and a check added to a route
//! later is decided again with the rest, without a list to keep in sync.
//! Writes from outside are decided again the same way: a bound action, an
//! HttpEndpoint's action bridge or admission, and a webhook. Their dispatch
//! answers `StaleWrite` (see `dispatch::outside_write`) where a kernel write's
//! answers the actor's refusal, and only a write still decided on a changed
//! record at the deadline answers it to the caller.

use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_runtime::scheduler::sim_now;

use super::ServerState;

/// How many turns a simulation may give other requests between a write's
/// decision and its dispatch (see `buggify_yield`): the window the
/// authorization precondition guards.
const DECISION_YIELD_BUDGET: u32 = 8;

/// One write's decisions: the first, and each one again on a changed record.
pub(crate) struct Redecide {
    /// Past this, a write decided on a changed record gets the 409 it got
    /// before: the server already bounds how long a write waits on its record
    /// (`action_dispatch_timeout`, the time an actor ask may take), and a
    /// record written that fast for that long is answered, not waited on.
    deadline: DateTime<Utc>,
    decisions: u32,
}

impl Redecide {
    /// The first decision of a write arriving now.
    pub(crate) fn start(state: &ServerState) -> Self {
        Self::until(sim_now(), state.action_dispatch_timeout)
    }

    fn until(now: DateTime<Utc>, wait: Duration) -> Self {
        let wait = chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::MAX);
        Self {
            deadline: now
                .checked_add_signed(wait)
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
            decisions: 1,
        }
    }

    /// Whether this is the write's first decision. Costs a write pays once,
    /// such as its rate-limit token, are taken on the first decision only.
    pub(crate) fn is_first(&self) -> bool {
        self.decisions == 1
    }

    /// Between the decision and the dispatch. A simulation runs other requests
    /// here, the window in which the record can change under the decision.
    pub(crate) async fn dispatching(&self) {
        temper_runtime::buggify::buggify_yield(DECISION_YIELD_BUDGET).await;
    }

    /// Whether to decide the write again: it was decided on a changed record
    /// (`stale`) and the deadline has not passed.
    pub(crate) fn again(&mut self, stale: bool, route: &'static str) -> bool {
        if !stale {
            return false;
        }
        let now = sim_now();
        if now >= self.deadline {
            tracing::warn!(
                route,
                decisions = self.decisions,
                "write still decided on a changed record at its deadline; answering 409"
            );
            return false;
        }
        self.decisions += 1;
        tracing::debug!(
            route,
            decisions = self.decisions,
            "record changed after the write was decided; deciding again"
        );
        true
    }
}

#[cfg(test)]
#[path = "redecide_test.rs"]
mod tests;
