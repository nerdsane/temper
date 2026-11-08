//! A state timer's fire, as whoever set it, with the standing they have on
//! its record when it fires (`Stake`), and the other moves the kernel sends
//! with no parameters.

use std::sync::Arc;

use temper_authz::May;

use super::super::senders::{OUTSIDE, Stake};
use super::super::{Call, Dispatch, JointState};
use super::commit::add;
use super::step::Certainty;
use super::{JointModel, Timeout};
use crate::model::{Fields, ParamValue};

impl JointModel {
    /// `entity`'s timer in `status`, when the model makes timers fire.
    pub(super) fn timeout(&self, entity: &str, status: &str) -> Option<&Timeout> {
        self.kernel
            .timers
            .get(entity)?
            .iter()
            .find(|timeout| timeout.state == status)
    }

    /// Where `entity`'s timer, firing `name`, leads from `state`: its move,
    /// or, when its setter could read the record as they set it and can no
    /// longer, nowhere, the timer spent. Where Cedar may answer either way,
    /// both.
    pub(super) fn fire_timer(
        &self,
        state: &JointState,
        entity: &str,
        name: &str,
    ) -> Vec<(JointState, String, Certainty)> {
        let armed = state.armed.get(entity).copied().unwrap_or(OUTSIDE);
        let (armer, stake) = Stake::of(armed);
        let by = self.senders.timer(armer);
        let text = if self.sender_matters.contains(entity) {
            format!("{entity}.{name} (timer, as {})", self.senders.name(by))
        } else {
            format!("{entity}.{name} (timer)")
        };
        let mut next = state.clone();
        next.armed.remove(entity);
        let now = match stake {
            Stake::None => May::Yes,
            Stake::Read | Stake::Either => {
                self.senders.may_read(armer, entity, &state.records[entity])
            }
        };
        let (fires, lost) = match (stake, now) {
            (Stake::None, _) | (_, May::Yes) => (Some(Certainty::Sure), None),
            (Stake::Read, May::No) => (None, Some(Certainty::Sure)),
            _ => (Some(Certainty::Either), Some(Certainty::Either)),
        };
        let mut out = Vec::new();
        if let Some(fires) = fires {
            let moved = self.fire(next.clone(), entity, name, &text, (by, None));
            out.extend(moved.into_iter().map(|(s, t, c)| (s, t, c.then(fires))));
        }
        if let Some(lost) = lost {
            let who = self.senders.name(by);
            out.push((
                next,
                format!("{text}: withheld, {who} may no longer read it"),
                lost,
            ));
        }
        out
    }

    /// The kernel sends `name` to `entity`'s record with no parameters, as
    /// `by` (a timer, or a module's `on_failure`): it goes through the
    /// dispatch path like any dispatch.
    pub(super) fn fire(
        &self,
        state: JointState,
        entity: &str,
        name: &str,
        text: &str,
        (by, caller): (u32, Option<Fields<ParamValue>>),
    ) -> Vec<(JointState, String, Certainty)> {
        let dispatch = Dispatch {
            to: entity.into(),
            action: name.into(),
            target_state: None,
            params: Fields::new(),
            read: !self.action_reads_related(entity, name),
            chain: Arc::new([]),
            via: None,
            by,
            caller,
            unread_guard: None,
        };
        if !dispatch.read {
            let mut next = state;
            add(&mut next, dispatch);
            return vec![(next, words!(self, "{text} sent"), Certainty::Sure)];
        }
        self.deliver(state, &dispatch, text)
    }

    /// The states a call that ended unanswered (`text`) leads to from
    /// `next`, where it is no longer in flight: its `on_failure` action, if
    /// it names one, sent as `by`, whoever made the move that called the
    /// module, in the chain the answer comes in.
    pub(super) fn end_call(
        &self,
        next: JointState,
        call: &Call,
        text: String,
        by: u32,
    ) -> Vec<(JointState, String, Certainty)> {
        match &call.on_failure {
            Some(name) => self.fire(
                next,
                &call.to,
                name,
                &words!(self, "{text}: {}.{name}", call.to),
                (by, call.caller.clone()),
            ),
            None => vec![(next, text, Certainty::Sure)],
        }
    }
}
