//! A state's outside actions, module answers, timers and dispatch moves.

use super::super::env::Blind;
use super::super::{Call, JointState};
use super::step::Move;
use super::{Delivery, JointModel};
use crate::model::semantics::successors_budgeted;
use crate::model::{ModelString, ParamValue, TemperModelAction};
use std::collections::BTreeSet;

impl JointModel {
    /// Every move from `state`: each record's enabled actions someone
    /// outside may send, as each sender that may, with string parameters chosen among the values held
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
                if self.outside_from(entity, &t.name).is_some()
                    && self.enabled(entity, t, state, Blind::Nothing)
                    && !self.outside_moves(entity, t, state, &values, &mut out)
                {
                    return out;
                }
                let senders = self.outside_senders(entity, &t.name);
                if senders.is_empty() || !self.enabled(entity, t, state, Blind::Nothing) {
                    continue;
                }
                let reads = self.reads_related(t);
                for (params, _) in successors_budgeted(model, t, local, &values, &self.expansion) {
                    let action = TemperModelAction {
                        name: t.name.clone(),
                        target_state: t.to_state.clone(),
                        params,
                    };
                    for &by in &senders {
                        let mv = if reads {
                            Move::Send(entity.clone(), action.clone(), by)
                        } else {
                            Move::Take(entity.clone(), action.clone(), by)
                        };
                        if !self.keep_move(&mut out, mv) {
                            return out;
                        }
                    }
                }
            }
            if state.armed.contains_key(entity)
                && let Some(timeout) = self.timeout(entity, &local.status)
            {
                let mv = Move::Timer(entity.clone(), timeout.action.clone());
                if !self.keep_move(&mut out, mv) {
                    return out;
                }
            }
        }
        let mut calls = BTreeSet::new();
        for call in &state.calls {
            if calls.insert(call)
                && !(self.answers(state, call, &values, &mut out)
                    && self.keep_move(&mut out, Move::Fail(call.clone()))
                    && self.keep_move(&mut out, Move::Lost(call.clone())))
            {
                return out;
            }
        }
        let mut seen = BTreeSet::new();
        for dispatch in &state.inflight {
            if seen.insert(dispatch) {
                let mv = if dispatch.read {
                    Move::Arrive(dispatch.clone())
                } else {
                    Move::Read(dispatch.clone())
                };
                if !self.keep_move(&mut out, mv) {
                    return out;
                }
            }
        }
        out
    }

    /// The answers a module may give the call `call`: its trigger's
    /// `on_success`, or else any internal action of its record. The module
    /// itself is not modelled: it may give each of these answers, and gives
    /// each it keeps being able to give at some point. False when the
    /// expansion's room is used up.
    fn answers(
        &self,
        state: &JointState,
        call: &Call,
        values: &BTreeSet<ModelString>,
        out: &mut Vec<Move>,
    ) -> bool {
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
                if !self.keep_move(out, Move::Answer(call.clone(), action)) {
                    return false;
                }
            }
        }
        true
    }
}
