//! A state's outside actions, module answers, timers and dispatch moves.

use super::super::env::Blind;
use super::super::{Call, Dispatch, JointState};
use super::{Delivery, JointModel};
use crate::model::semantics::inputs_within_bounds;
use crate::model::{ModelString, ParamValue, TemperModelAction};
use std::collections::BTreeSet;
use std::sync::Arc;

/// One move of the walk.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::composite::joint) enum Move {
    /// An action whose guard and constraints read only its own record, sent
    /// from outside by a sender: its actor checks them as it writes, so
    /// sending and taking are one step.
    Take(String, TemperModelAction, u32),
    /// An action whose guard reads related records, sent from outside by a
    /// sender: the dispatch path read them and they allowed it; its actor
    /// takes it later.
    Send(String, TemperModelAction, u32),
    /// A reaction from a type outside the scope, as the sender it runs as:
    /// a move from outside the model, whenever its target's guard may let
    /// it ([`from_outside::Outside`]).
    Reaction(String, TemperModelAction, u32),
    /// A record's state timer fires its action, with no parameters, as
    /// whoever armed it.
    Timer(String, String),
    /// A module answers the call a record's trigger started.
    Answer(Arc<Call>, TemperModelAction),
    /// A module's call fails: its `on_failure` action, if any, is sent.
    Fail(Arc<Call>),
    /// A module's call is lost: the server restarts while it runs. Its
    /// deadline answers it with its `on_failure` action, if it names one,
    /// as a failed call is answered; otherwise nothing answers it.
    Lost(Arc<Call>),
    /// A module's call ends with no answer: it succeeded and no move follows
    /// (the kernel's `integration answered no move`, an empty answer, or no
    /// named answer fits), and its `on_failure` is not for that. The record
    /// stays where it is. Any call may end this way, whatever it names.
    Silent(Arc<Call>),
    /// The dispatch path reads the related records a reaction needs.
    Read(Arc<Dispatch>),
    /// The target's actor takes a dispatch, or refuses it.
    Arrive(Arc<Dispatch>),
}

impl Move {
    /// Whether the system makes this move (a timer, a module, or the
    /// kernel's dispatches) rather than someone outside or a restart.
    pub(in crate::composite::joint) fn by_system(&self) -> bool {
        !matches!(
            self,
            Move::Take(..) | Move::Send(..) | Move::Reaction(..) | Move::Lost(..)
        )
    }

    /// Whether this is a reaction from a type outside the scope.
    pub(in crate::composite::joint) fn is_outside_reaction(&self) -> bool {
        matches!(self, Move::Reaction(..))
    }

    /// The entity and action an outside move sends.
    pub(in crate::composite::joint) fn outside_action(&self) -> Option<(&str, &str)> {
        match self {
            Move::Take(entity, action, _) | Move::Send(entity, action, _) => {
                Some((entity, &action.name))
            }
            _ => None,
        }
    }
}

/// What is next for a dispatch in flight: its related records are read, then
/// it lands.
pub(in crate::composite::joint) fn dispatch_move(dispatch: &Arc<Dispatch>) -> Move {
    if dispatch.read && dispatch.unread_guard.is_none() {
        Move::Arrive(dispatch.clone())
    } else {
        Move::Read(dispatch.clone())
    }
}

impl JointModel {
    /// Every move from `state`: each record's enabled actions someone
    /// outside may send, as each sender that may, with string parameters chosen among the values held
    /// anywhere in the joint state, and each state timer due (when its
    /// delivery lets a move from outside come now); each module call's
    /// answers and failure; and each dispatch in flight.
    pub(in crate::composite::joint) fn moves(&self, state: &JointState) -> Vec<Move> {
        let values = self.values(state);
        let mut out = Vec::new();
        if self.asked(state) && !self.record_moves(state, &values, &mut out) {
            return out;
        }
        let mut calls = BTreeSet::new();
        for call in &state.calls {
            if calls.insert(call) && !self.call_moves(state, call, &values, &mut out) {
                return out;
            }
        }
        let mut seen = BTreeSet::new();
        for dispatch in &state.inflight {
            if seen.insert(dispatch) && !self.keep_move(&mut out, dispatch_move(dispatch)) {
                return out;
            }
        }
        out
    }

    /// Whether a move from outside may come now.
    pub(in crate::composite::joint) fn asked(&self, state: &JointState) -> bool {
        self.delivery == Delivery::Any || state.inflight.is_empty()
    }

    /// The records' moves: what someone outside may send, and the timers due.
    /// False when the expansion's room is used up.
    pub(in crate::composite::joint) fn record_moves(
        &self,
        state: &JointState,
        values: &BTreeSet<ModelString>,
        out: &mut Vec<Move>,
    ) -> bool {
        for (entity, model) in self.models.iter() {
            let local = &state.records[entity.as_str()];
            for t in &model.transitions {
                if !self.expansion.take() {
                    return false;
                }
                if self.outside_from(entity, &t.name).is_some()
                    && self.enabled(entity, t, state, Blind::Nothing)
                    && !self.outside_moves(entity, t, state, values, out)
                {
                    return false;
                }
                let senders = self.outside_senders(entity, &t.name);
                if senders.is_empty() || !self.enabled(entity, t, state, Blind::Nothing) {
                    continue;
                }
                let reads = self.reads_related(t);
                for params in inputs_within_bounds(model, t, local, values, &self.expansion) {
                    let action = TemperModelAction {
                        name: t.name,
                        target_state: t.to_state,
                        params,
                    };
                    for &by in senders.iter() {
                        let mv = if reads {
                            Move::Send(entity.clone(), action.clone(), by)
                        } else {
                            Move::Take(entity.clone(), action.clone(), by)
                        };
                        if !self.keep_move(out, mv) {
                            return false;
                        }
                    }
                }
            }
            if state.armed.contains_key(entity.as_str())
                && let Some(timeout) = self.timeout(entity, &local.status)
            {
                let mv = Move::Timer(entity.clone(), timeout.action.clone());
                if !self.keep_move(out, mv) {
                    return false;
                }
            }
        }
        true
    }

    /// The ways a module's call may end. Naming its answers narrows what a
    /// trigger may answer; any module may still answer nothing (`"empty":
    /// true`), and then the kernel makes no move. So a call may always end
    /// with none, and each state it can leave a record in needs a way out of
    /// its own. False when the expansion's room is used up.
    pub(in crate::composite::joint) fn call_moves(
        &self,
        state: &JointState,
        call: &Arc<Call>,
        values: &BTreeSet<ModelString>,
        out: &mut Vec<Move>,
    ) -> bool {
        self.answers(state, call, values, out)
            && self.keep_move(out, Move::Fail(call.clone()))
            && self.keep_move(out, Move::Lost(call.clone()))
            && self.keep_move(out, Move::Silent(call.clone()))
    }

    /// Whether `action` of `entity` is one nothing else can send, on a record
    /// a module whose trigger names no `on_success` may answer: no request,
    /// reaction or timer. It may be how that module answers, or never happen.
    pub(super) fn only_a_module_may(&self, entity: &str, action: &str) -> bool {
        self.unsent.get(entity).is_some_and(|ways| {
            ways.iter()
                .any(|&way| self.models[entity].transitions[way].name == action)
        })
    }

    /// The answers a module may give the call `call`: its trigger's
    /// `on_success`, or else any internal action of its record, and any
    /// action of its record nothing else can send
    /// ([`Self::only_a_module_may`]). The module itself is not modelled: it
    /// may give each of these answers, and gives each internal or named one
    /// it keeps being able to give at some point. False when the expansion's
    /// room is used up.
    fn answers(
        &self,
        state: &JointState,
        call: &std::sync::Arc<Call>,
        values: &BTreeSet<ModelString>,
        out: &mut Vec<Move>,
    ) -> bool {
        let model = &self.models[call.to.as_str()];
        let local = &state.records[call.to.as_str()];
        for t in &model.transitions {
            let answers = if call.on_success.is_empty() {
                self.kernel
                    .internal
                    .get(call.to.as_str())
                    .is_some_and(|names| names.contains(t.name.as_str()))
                    || self.only_a_module_may(&call.to, &t.name)
            } else {
                call.on_success.contains(&t.name)
            };
            if !answers || !self.enabled(&call.to, t, state, Blind::Nothing) {
                continue;
            }
            let mut seen = BTreeSet::new();
            for mut params in inputs_within_bounds(model, t, local, values, &self.expansion) {
                // A parameter tied to a field carries it as the call found it.
                for param in &t.string_params {
                    if let Some(value) = param.equals.iter().find_map(|f| call.asked.get(f)) {
                        params.insert(param.name, ParamValue::Str(*value));
                    }
                }
                if !seen.insert(params.clone()) {
                    continue;
                }
                let action = TemperModelAction {
                    name: t.name,
                    target_state: t.to_state,
                    params,
                };
                if !self.keep_move(out, Move::Answer(call.clone(), action)) {
                    return false;
                }
            }
        }
        true
    }
}
