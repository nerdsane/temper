//! The moves of the joint model and the states they lead to: actions sent
//! from outside, state timers, module answers, dispatches read on the
//! dispatch path, and dispatches their records take or refuse.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::May;
use temper_spec::predicate::{Truth, eval};

use super::super::env::Blind;
use super::super::senders::{OUTSIDE, Rule};
use super::super::state::canonical;
use super::super::{Call, Dispatch, JointState};
use super::commit::add;
use super::labels::rename_label;
use super::{Delivery, JointModel, MAX_IN_FLIGHT};
use crate::model::semantics::successors_budgeted;
use crate::model::{ModelString, ParamValue, TemperModelAction};

/// One move of the walk.
#[derive(Debug, Clone)]
pub(in crate::composite::joint) enum Move {
    /// An action whose guard and constraints read only its own record: its
    /// actor checks them as it writes, so sending and taking are one step.
    Take(String, TemperModelAction),
    /// An action whose guard reads related records, sent from outside: the
    /// dispatch path read them and they allowed it; its actor takes it
    /// later.
    Send(String, TemperModelAction),
    /// A record's state timer fires its action, with no parameters, as
    /// whoever armed it.
    Timer(String, String),
    /// A module answers the call a record's trigger started.
    Answer(Call, TemperModelAction),
    /// A module's call fails: its `on_failure` action, if any, is sent.
    Fail(Call),
    /// A module's call is lost: the server restarts while it runs, and
    /// nothing answers it.
    Lost(Call),
    /// The dispatch path reads the related records a reaction needs.
    Read(Dispatch),
    /// The target's actor takes a dispatch, or refuses it.
    Arrive(Dispatch),
}

impl Move {
    /// Whether the system makes this move (a timer, a module, or the
    /// kernel's dispatches) rather than someone outside or a restart.
    pub(in crate::composite::joint) fn by_system(&self) -> bool {
        !matches!(self, Move::Take(..) | Move::Send(..) | Move::Lost(..))
    }

    /// The entity and action an outside move sends.
    pub(in crate::composite::joint) fn outside_action(&self) -> Option<(&str, &str)> {
        match self {
            Move::Take(entity, action) | Move::Send(entity, action) => Some((entity, &action.name)),
            _ => None,
        }
    }
}

/// What the model knows about one state a move leads to. The order is
/// from most to least known; a step after a step knows the lesser of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::composite::joint) enum Certainty {
    /// The move can lead here.
    Sure,
    /// The move leads to one of the states it lists with this, and the model
    /// cannot tell which: a guard or constraint it cannot decide, taken and
    /// refused, or Cedar that may refuse.
    OneOf,
    /// The move may lead here, and may also end somewhere it does not list.
    Unsure,
}

impl Certainty {
    /// What is known of a step followed by `then`.
    pub(in crate::composite::joint) fn then(self, then: Certainty) -> Certainty {
        self.max(then)
    }

    /// `Sure` when `known`, else `fallback`.
    pub(in crate::composite::joint) fn unless(known: bool, fallback: Certainty) -> Certainty {
        if known { Certainty::Sure } else { fallback }
    }
}

/// A state a move leads to.
pub(in crate::composite::joint) struct Next {
    pub(in crate::composite::joint) state: JointState,
    /// The new names of the values the old state held.
    pub(in crate::composite::joint) renamed: BTreeMap<ModelString, ModelString>,
    /// The move, named as in the new state.
    pub(in crate::composite::joint) label: String,
    /// What the model knows of the move leading here.
    pub(in crate::composite::joint) certainty: Certainty,
}

impl JointModel {
    /// Every move from `state`: each record's enabled actions someone
    /// outside may send, with string parameters chosen among the values held
    /// anywhere in the joint state, and each state timer due (when its
    /// delivery lets a move from outside come now); each module call's
    /// answers and failure; and each dispatch in flight.
    pub(in crate::composite::joint) fn moves(&self, state: &JointState) -> Vec<Move> {
        let values = self.values(state);
        let mut out = Vec::new();
        let asked = self.delivery == Delivery::Any || state.inflight.is_empty();
        for (entity, model) in self.models.iter().filter(|_| asked) {
            let local = &state.records[entity];
            for t in &model.transitions {
                if !self.expansion.take() {
                    return out;
                }
                if !self.sendable(entity, &t.name)
                    || !self.enabled(entity, t, state, Blind::Nothing)
                {
                    continue;
                }
                let reads = self.reads_related(t);
                for (params, _) in successors_budgeted(model, t, local, &values, &self.expansion) {
                    let action = TemperModelAction {
                        name: t.name.clone(),
                        target_state: t.to_state.clone(),
                        params,
                    };
                    out.push(if reads {
                        Move::Send(entity.clone(), action)
                    } else {
                        Move::Take(entity.clone(), action)
                    });
                }
            }
            if state.armed.contains_key(entity)
                && let Some(timeout) = self.timeout(entity, &local.status)
            {
                out.push(Move::Timer(entity.clone(), timeout.action.clone()));
            }
        }
        let mut calls = BTreeSet::new();
        for call in &state.calls {
            if calls.insert(call) {
                self.answers(state, call, &values, &mut out);
                out.push(Move::Fail(call.clone()));
                out.push(Move::Lost(call.clone()));
            }
        }
        let mut seen = BTreeSet::new();
        for dispatch in &state.inflight {
            if seen.insert(dispatch) {
                out.push(if dispatch.read {
                    Move::Arrive(dispatch.clone())
                } else {
                    Move::Read(dispatch.clone())
                });
            }
        }
        out
    }

    /// The states `mv` leads to from `state`, each with what the model knows
    /// of it, and whether a state was held back because it would put more
    /// than [`MAX_IN_FLIGHT`] dispatches in flight.
    pub(in crate::composite::joint) fn next(
        &self,
        state: &JointState,
        mv: &Move,
    ) -> (Vec<Next>, bool) {
        if !self.expansion.take() {
            return (Vec::new(), false);
        }
        use Certainty::{OneOf, Sure, Unsure};
        let mut outcomes: Vec<(JointState, String, Certainty)> = Vec::new();
        match mv {
            Move::Take(entity, action) => {
                // An outside move the model cannot decide may also be
                // refused, which leaves the state as it is.
                let known = self.truth(entity, action, state) == Truth::True
                    && self.outside_may(entity, &action.name) == May::Yes
                    && self.unchecked.meets(entity, &action.name)
                    && self.exact(entity, &action.name, state);
                let text = format!("{entity}.{action}");
                for (outcome, then) in self.commit(state.clone(), entity, action, &[], OUTSIDE) {
                    let certainty = Certainty::unless(known, Unsure).then(then);
                    outcomes.push((outcome, text.clone(), certainty));
                }
            }
            Move::Send(entity, action) => {
                // Its record's guard is read on the dispatch path and in its
                // actor, which decide it later.
                let known = self.outside_may(entity, &action.name) == May::Yes;
                let mut next = state.clone();
                add(&mut next, sent(entity, action, OUTSIDE));
                let text = format!("{entity}.{action} sent");
                outcomes.push((next, text, Certainty::unless(known, Unsure)));
            }
            Move::Timer(entity, name) => {
                let armed = state.armed.get(entity).copied().unwrap_or(OUTSIDE);
                let by = self.senders.timer(armed);
                let text = if self.sender_matters.contains(entity) {
                    format!("{entity}.{name} (timer, as {})", self.senders.name(by))
                } else {
                    format!("{entity}.{name} (timer)")
                };
                let mut next = state.clone();
                next.armed.remove(entity);
                outcomes.extend(self.fire(next, entity, name, &text, by));
            }
            Move::Answer(call, action) => {
                let mut next = state.clone();
                remove_call(&mut next, call);
                let text = format!(
                    "{}.{action} (answer from {}, as {})",
                    call.to,
                    call.module,
                    self.senders.name(call.by)
                );
                if self.action_reads_related(&call.to, &action.name) {
                    add(&mut next, sent(&call.to, action, call.by));
                    outcomes.push((next, format!("{text} sent"), Sure));
                } else if self.stale(call, &action.name, state) {
                    // The module answers about the record as the call found
                    // it, and the record has moved on from that.
                    outcomes.push((next, format!("{text} refused by its record"), Sure));
                } else {
                    // The record refuses an answer whose guard or constraints
                    // do not hold, and the call ends there
                    // (`record_generated_callback_refusal`).
                    let known = self.truth(&call.to, action, state) == Truth::True
                        && self.unchecked.meets(&call.to, &action.name)
                        && self.exact(&call.to, &action.name, state);
                    if !known {
                        let refused = format!("{text} refused by its record");
                        outcomes.push((next.clone(), refused, OneOf));
                    }
                    for (outcome, then) in self.commit(next, &call.to, action, &[], call.by) {
                        let certainty = Certainty::unless(known, OneOf).then(then);
                        outcomes.push((outcome, text.clone(), certainty));
                    }
                }
            }
            Move::Fail(call) => {
                let mut next = state.clone();
                remove_call(&mut next, call);
                let text = format!("{call} fails");
                match &call.on_failure {
                    Some(name) => outcomes.extend(self.fire(
                        next,
                        &call.to,
                        name,
                        &format!("{text}: {}.{name}", call.to),
                        call.by,
                    )),
                    None => outcomes.push((next, text, Sure)),
                }
            }
            Move::Lost(call) => {
                let mut next = state.clone();
                remove_call(&mut next, call);
                outcomes.push((next, format!("{call} is lost: the server restarts"), Sure));
            }
            Move::Read(dispatch) => {
                let mut next = state.clone();
                remove(&mut next, dispatch);
                let allows = self.read_allows(dispatch, state);
                if allows != Truth::True {
                    let refused = format!("{dispatch} refused when read");
                    let certainty = Certainty::unless(allows == Truth::False, OneOf);
                    outcomes.push((next.clone(), refused, certainty));
                }
                if allows != Truth::False {
                    let mut read = dispatch.clone();
                    read.read = true;
                    add(&mut next, read);
                    let text = format!("{dispatch}: related records read");
                    outcomes.push((next, text, Certainty::unless(allows == Truth::True, OneOf)));
                }
            }
            Move::Arrive(dispatch) => {
                let mut next = state.clone();
                remove(&mut next, dispatch);
                // Cedar decides a reaction as its sender; every other
                // dispatch was decided at its entry point, or is the kernel's.
                let may = if let Some(index) = dispatch.via {
                    let reaction = &self.reactions[index];
                    let rule = Rule {
                        from: &reaction.from,
                        action: &reaction.source_action,
                        trigger: &reaction.trigger,
                    };
                    self.senders
                        .may_in(dispatch.by, &rule, &dispatch.to, &dispatch.action, state)
                } else {
                    May::Yes
                };
                if may != May::Yes {
                    let text = format!(
                        "{dispatch} refused by Cedar for {}",
                        self.senders.name(dispatch.by)
                    );
                    outcomes.push((next.clone(), text, Certainty::unless(may == May::No, OneOf)));
                }
                if may != May::No {
                    let cedar = Certainty::unless(may == May::Yes, OneOf);
                    outcomes.extend(
                        self.deliver(next, dispatch, &dispatch.to_string())
                            .into_iter()
                            .map(|(s, t, then)| (s, t, cedar.then(then))),
                    );
                }
            }
        }
        let mut held_back = false;
        let nexts = outcomes
            .into_iter()
            .filter(|(outcome, _, _)| {
                let fits = self.delivery == Delivery::BeforeNextRequest
                    || outcome.inflight.len() + outcome.calls.len() <= MAX_IN_FLIGHT;
                held_back |= !fits;
                fits
            })
            .map(|(mut outcome, text, certainty)| {
                self.order.normalize(&mut outcome);
                let renamed = canonical(&mut outcome);
                Next {
                    state: outcome,
                    label: rename_label(&text, mv, &renamed),
                    renamed,
                    certainty,
                }
            })
            .collect();
        (nexts, held_back)
    }

    /// Whether the dispatch path lets `dispatch` through: the related
    /// records its guard reads allow it, for one of the transitions it may
    /// take. Its own record is read later, by its actor. True when a guard
    /// holds outright, false when the related records refuse every one, and
    /// unknown when the model cannot tell.
    pub(super) fn read_allows(&self, dispatch: &Dispatch, state: &JointState) -> Truth {
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
    pub(super) fn exact(&self, entity: &str, action: &str, state: &JointState) -> bool {
        self.models[entity]
            .transitions
            .iter()
            .filter(|t| t.name == action)
            .all(|t| self.order.exact(entity, &t.effects, state))
    }

    /// Whether a field `action` ties a parameter to has changed since `call`
    /// started, so its record refuses the module's answer.
    fn stale(&self, call: &Call, action: &str, state: &JointState) -> bool {
        let strings = &state.records[&call.to].strings;
        self.unchecked
            .equals
            .get(&(call.to.clone(), action.to_string()))
            .is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| call.asked.get(field) != strings.get(field))
            })
    }

    /// Whether someone outside may send `action` to an `entity` record.
    fn sendable(&self, entity: &str, action: &str) -> bool {
        self.outside_may(entity, action) != May::No
    }

    /// What Cedar lets someone outside send; an action outside code is
    /// relied on to send is taken as allowed for whoever sends it.
    fn outside_may(&self, entity: &str, action: &str) -> May {
        if self
            .relies_on
            .contains(&(entity.to_string(), action.to_string()))
        {
            return May::Yes;
        }
        self.senders.request(entity, action)
    }

    /// Whether the guard of the transition `action` names holds on `state`,
    /// as far as the model knows.
    fn truth(&self, entity: &str, action: &TemperModelAction, state: &JointState) -> Truth {
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

    /// The answers a module may give the call `call`: its trigger's
    /// `on_success`, or else any internal action of its record. The module
    /// itself is not modelled: it may give each of these answers, and gives
    /// each it keeps being able to give at some point.
    fn answers(
        &self,
        state: &JointState,
        call: &Call,
        values: &BTreeSet<ModelString>,
        out: &mut Vec<Move>,
    ) {
        let model = &self.models[&call.to];
        let local = &state.records[&call.to];
        for t in &model.transitions {
            let answers = if call.on_success.is_empty() {
                self.kernel
                    .internal
                    .get(&call.to)
                    .is_some_and(|names| names.contains(&t.name))
            } else {
                call.on_success.contains(&t.name)
            };
            if !answers || !self.enabled(&call.to, t, state, Blind::Nothing) {
                continue;
            }
            let mut seen = BTreeSet::new();
            for (mut params, _) in successors_budgeted(model, t, local, values, &self.expansion) {
                // A parameter tied to a field carries it as the call found it.
                for param in &t.string_params {
                    if let Some(value) = param.equals.iter().find_map(|f| call.asked.get(f)) {
                        params.insert(param.name.clone(), ParamValue::Str(value.clone()));
                    }
                }
                if !seen.insert(params.clone()) {
                    continue;
                }
                let action = TemperModelAction {
                    name: t.name.clone(),
                    target_state: t.to_state.clone(),
                    params,
                };
                out.push(Move::Answer(call.clone(), action));
            }
        }
    }
}

/// The dispatch an outside sender, or a module's answer, makes after the
/// dispatch path read the related records and they allowed it.
fn sent(entity: &str, action: &TemperModelAction, by: u32) -> Dispatch {
    Dispatch {
        to: entity.to_string(),
        action: action.name.clone(),
        target_state: action.target_state.clone(),
        params: action.params.clone(),
        read: true,
        chain: Vec::new(),
        via: None,
        by,
    }
}

pub(super) fn remove(state: &mut JointState, dispatch: &Dispatch) {
    if let Some(at) = state.inflight.iter().position(|d| d == dispatch) {
        state.inflight.remove(at);
    }
}

fn remove_call(state: &mut JointState, call: &Call) {
    if let Some(at) = state.calls.iter().position(|c| c == call) {
        state.calls.remove(at);
    }
}
