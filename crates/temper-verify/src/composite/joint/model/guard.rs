//! A trigger's guard on related records in scope, read when its dispatch is
//! read: the runtime evaluates it after its source commits, on the related
//! records as they are then.

use std::sync::Arc;

use temper_spec::predicate::{Truth, eval};

use super::super::super::env::Blind;
use super::super::super::{Call, Dispatch, JointState};
use super::super::JointModel;
use super::super::commit::add;
use super::{Certainty, MAX_IN_FLIGHT, remove};
use crate::model::TemperModelAction;

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
            let text = words!(self, "{dispatch}: its trigger's guard does not hold");
            let certainty = Certainty::unless(holds == Truth::False, Certainty::OneOf);
            outcomes.push((next.clone(), text, certainty));
        }
        if holds != Truth::False {
            let mut sent = dispatch.clone();
            sent.unread_guard = None;
            add(&mut next, sent);
            let text = words!(self, "{dispatch}: its trigger's guard holds");
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
        env.params = Some(&committed.params);
        env.unheld_unknown = true;
        eval(guard, &env)
    }

    /// Whether `state` holds no more in flight than the walk keeps: at most
    /// [`MAX_IN_FLIGHT`] things the runtime is working on at once. A
    /// dispatch or call it has sent is one. A commit whose reactions wait
    /// for their guards to be read is one too, however many wait: the
    /// runtime reads them one trigger at a time for that commit
    /// (`temper-server` `trigger::dispatcher`). Copies of one commit's
    /// waiting reactions count as the most copies of any one of them.
    pub(in crate::composite::joint) fn fits<'a>(&'a self, state: &'a JointState) -> bool {
        // A commit's waiting reactions count at most one each.
        if state.calls.len() + state.inflight.len() <= MAX_IN_FLIGHT {
            return true;
        }
        let waiting = |dispatch: &'a Arc<Dispatch>| match (
            &dispatch.unread_guard,
            dispatch.via,
            dispatch.chain.split_last(),
        ) {
            (Some(_), Some(index), Some((_, parent))) => {
                let reaction = &self.reactions[index];
                Some(((parent, &reaction.from, &reaction.source_action), index))
            }
            _ => None,
        };
        let inflight = &state.inflight;
        let mut busy = state.calls.len();
        for (at, dispatch) in inflight.iter().enumerate() {
            let Some((commit, _)) = waiting(dispatch) else {
                busy += 1;
                continue;
            };
            // A commit counts once, when first met: as the most dispatches
            // any one of its reactions has waiting.
            if inflight[..at]
                .iter()
                .any(|earlier| waiting(earlier).is_some_and(|(other, _)| other == commit))
            {
                continue;
            }
            let of_commit = || {
                inflight[at..]
                    .iter()
                    .filter_map(&waiting)
                    .filter(|(other, _)| *other == commit)
                    .map(|(_, index)| index)
            };
            busy += of_commit()
                .map(|index| of_commit().filter(|other| *other == index).count())
                .max()
                .unwrap_or(0);
        }
        busy <= MAX_IN_FLIGHT
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
    pub(in crate::composite::joint) fn overtakes(
        &self,
        state: &JointState,
        dispatch: &Dispatch,
    ) -> bool {
        let (Some(index), Some((_, parent))) = (dispatch.via, dispatch.chain.split_last()) else {
            return false;
        };
        let reaction = &self.reactions[index];
        // Spawns run beside a commit's reactions, not in their order.
        if reaction.spawn.is_some() {
            return false;
        }
        let earlier = |entity: &str, action: &str| {
            self.reactions[..index].iter().any(|r| {
                r.spawn.is_none()
                    && r.from == reaction.from
                    && r.source_action == reaction.source_action
                    && r.to == entity
                    && r.target_action == action
            })
        };
        let sibling = |other: &Dispatch| {
            other.via.is_some_and(|at| {
                at < index
                    && self.reactions[at].spawn.is_none()
                    && self.reactions[at].from == reaction.from
                    && self.reactions[at].source_action == reaction.source_action
            })
        };
        state.inflight.iter().any(|other| {
            **other != *dispatch
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

    /// Whether the dispatch path lets `dispatch` through: the related
    /// records its guard reads allow it, for one of the transitions it may
    /// take. Its own record is read later, by its actor. True when a guard
    /// holds outright, false when the related records refuse every one, and
    /// unknown when the model cannot tell.
    pub(in crate::composite::joint) fn read_allows(
        &self,
        dispatch: &Dispatch,
        state: &JointState,
    ) -> Truth {
        let full = self.env(&dispatch.to, state);
        let related = self.env_blind(&dispatch.to, state, Blind::Own);
        let mut refused = true;
        for t in self.transitions_for(dispatch) {
            if eval(&t.guard, &full) == Truth::True {
                return Truth::True;
            }
            refused &= eval(&t.guard, &related) == Truth::False;
        }
        if refused {
            Truth::False
        } else {
            Truth::Unknown
        }
    }

    /// Whether the counts kept as sequence numbers stay exact when `entity`
    /// takes `action` in `state`.
    pub(in crate::composite::joint) fn exact(
        &self,
        entity: &str,
        action: &str,
        state: &JointState,
    ) -> bool {
        self.models[entity]
            .transitions
            .iter()
            .filter(|t| t.name == action)
            .all(|t| self.order.exact(entity, &t.effects, state))
    }

    /// Whether a field `action` ties a parameter to has changed since `call`
    /// started, so its record refuses the module's answer.
    pub(super) fn stale(&self, call: &Call, action: &str, state: &JointState) -> bool {
        let strings = &state.records[call.to.as_str()].strings;
        self.unchecked
            .equals
            .get(pair!(&call.to, action))
            .is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| call.asked.get(field) != strings.get(field))
            })
    }

    /// Whether the guard of the transition `action` names holds on `state`,
    /// as far as the model knows.
    /// Whether all that is open of an outside move in `state` is what Cedar
    /// answers, or a guard on its own record's fields, which a sender can
    /// meet either way.
    pub(in crate::composite::joint) fn take_rests_on_cedar_or_own_guard(
        &self,
        entity: &str,
        action: &TemperModelAction,
        state: &JointState,
    ) -> bool {
        let guard = match self.truth(entity, action, state) {
            Truth::True => true,
            Truth::False => false,
            Truth::Unknown => !self.action_reads_related(entity, &action.name),
        };
        guard
            && self.exceeds(entity, &action.name, &action.params, &state.records[entity])
                == Truth::True
            && self.unchecked.meets(entity, &action.name)
            && self.exact(entity, &action.name, state)
    }

    pub(super) fn truth(
        &self,
        entity: &str,
        action: &TemperModelAction,
        state: &JointState,
    ) -> Truth {
        let status = &state.records[entity].status;
        self.models[entity]
            .transitions
            .iter()
            .find(|t| {
                t.name == action.name
                    && t.to_state == action.target_state
                    && (t.from_states.is_empty() || t.from_states.contains(status))
            })
            .map_or(Truth::Unknown, |t| eval(&t.guard, &self.env(entity, state)))
    }
}
