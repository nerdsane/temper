//! A trigger's guard on related records in scope, read when its dispatch is
//! read: the runtime evaluates it after its source commits, on the related
//! records as they are then.

use temper_spec::predicate::{Truth, eval};

use super::super::super::{Dispatch, JointState};
use super::super::JointModel;
use super::super::commit::add;
use super::{Certainty, MAX_IN_FLIGHT, remove};

impl JointModel {
    /// The states reading `dispatch`'s trigger guard leads to: dropped when
    /// it does not hold, sent on when it does, and both when it may.
    pub(super) fn read_trigger_guard(
        &self,
        dispatch: &Dispatch,
        state: &JointState,
    ) -> Vec<(JointState, String, Certainty)> {
        let mut outcomes = Vec::new();
        let mut next = state.clone();
        remove(&mut next, dispatch);
        let holds = self.trigger_guard(dispatch, state);
        if holds != Truth::True {
            let text = format!("{dispatch}: its trigger's guard does not hold");
            let certainty = Certainty::unless(holds == Truth::False, Certainty::OneOf);
            outcomes.push((next.clone(), text, certainty));
        }
        if holds != Truth::False {
            let mut sent = dispatch.clone();
            sent.unread_guard = None;
            add(&mut next, sent);
            let text = format!("{dispatch}: its trigger's guard holds");
            let certainty = Certainty::unless(holds == Truth::True, Certainty::OneOf);
            outcomes.push((next, text, certainty));
        }
        outcomes
    }

    /// Whether the guard of the trigger that sent `dispatch` holds on the
    /// related records as `state` holds them, read on its source as it
    /// committed. The model holds one record of each type: a reference that
    /// names another record (one of its source's own type, or one its id
    /// field moved to) may find one in the runtime, so it reads as unknown.
    /// Before this read, every trigger guard on a related record went both
    /// ways; only the records the model holds decide one.
    fn trigger_guard(&self, dispatch: &Dispatch, state: &JointState) -> Truth {
        let (Some(committed), Some(index)) = (&dispatch.unread_guard, dispatch.via) else {
            return Truth::True;
        };
        let reaction = &self.reactions[index];
        let Some(guard) = &reaction.guard else {
            return Truth::True;
        };
        let own = committed.record();
        let mut env = self.env(&reaction.from, state);
        env.own = Some(&own);
        env.unheld_unknown = true;
        eval(guard, &env)
    }

    /// Whether `state` holds no more in flight than the walk keeps: at most
    /// [`MAX_IN_FLIGHT`] dispatches and calls the runtime has sent, and at
    /// most one commit's worth of reactions whose guards are not read yet.
    /// A guard not yet read sends nothing until it holds, so it is not a
    /// dispatch in flight.
    pub(in crate::composite::joint) fn fits(&self, state: &JointState) -> bool {
        let unread = state
            .inflight
            .iter()
            .filter(|dispatch| dispatch.unread_guard.is_some())
            .count();
        state.inflight.len() - unread + state.calls.len() <= MAX_IN_FLIGHT && unread <= self.fan_out
    }

    /// Whether reading or landing `dispatch` now may come before an earlier
    /// reaction of the same commit, or that reaction's cascade, has landed.
    /// The runtime sends a commit's reactions one trigger at a time, in
    /// declaration order, each with its cascade before the next trigger's
    /// guard is read (`temper-server` `trigger::dispatcher`). The walk lets
    /// them land in any order, so a way that takes a later one first is one
    /// it cannot be sure of. A sibling in flight is earlier when its own
    /// reaction is. Deeper in a chain that runs through the same steps, a
    /// dispatch whose next step an earlier trigger of the same source action
    /// takes may be in that reaction's cascade.
    pub(super) fn overtakes(&self, state: &JointState, dispatch: &Dispatch) -> bool {
        let (Some(index), Some((_, parent))) = (dispatch.via, dispatch.chain.split_last()) else {
            return false;
        };
        let reaction = &self.reactions[index];
        let earlier = |entity: &str, action: &str| {
            self.reactions[..index].iter().any(|r| {
                r.from == reaction.from
                    && r.source_action == reaction.source_action
                    && r.to == entity
                    && r.target_action == action
            })
        };
        let sibling = |other: &Dispatch| {
            other.via.is_some_and(|at| {
                at < index
                    && self.reactions[at].from == reaction.from
                    && self.reactions[at].source_action == reaction.source_action
            })
        };
        state.inflight.iter().any(|other| {
            other != dispatch
                && other.chain.starts_with(parent)
                && match other.chain.len() - parent.len().min(other.chain.len()) {
                    0 => false,
                    1 => sibling(other),
                    _ => earlier(
                        &other.chain[parent.len()].entity_type,
                        &other.chain[parent.len()].action,
                    ),
                }
        })
    }
}
