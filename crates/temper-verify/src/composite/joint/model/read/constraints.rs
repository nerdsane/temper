//! The action constraints the joint model can decide and the values a
//! module call keeps to decide its answers.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{ActionConstraint, Automaton};
use temper_spec::predicate::VarKind;

use super::super::order::Order;
use crate::model::TemperModel;

/// The actions (entity, action) with a constraint the model cannot decide,
/// and what a module call keeps to decide its answer's.
#[derive(Debug, Default)]
pub(in crate::composite::joint::model) struct Unchecked {
    /// Sent as a dispatch or a request: one on a counter the order does not
    /// keep, or on a string parameter or field the model gives no value.
    pub(in crate::composite::joint::model) taken: BTreeSet<(String, String)>,
    /// Sent with parameters its sender chooses (a module's answer, or a
    /// request from outside): one that ties a parameter to a field the model
    /// does not track. Any other constraint the sender can meet. A module
    /// answers about the record as it was when the call started, so a call
    /// keeps the tracked fields its answers are tied to (`asked`).
    pub(in crate::composite::joint::model) chosen: BTreeSet<(String, String)>,
    /// Each (entity, action)'s tracked fields a parameter must equal.
    pub(in crate::composite::joint::model) equals: BTreeMap<(String, String), BTreeSet<String>>,
    /// Each (entity, action)'s count parameters that must equal a counter
    /// the model holds as a value (not as a sequence number), with that
    /// counter. Decided when a dispatch carries the count, and only then.
    pub(in crate::composite::joint::model) counts:
        BTreeMap<(String, String), Vec<(String, String)>>,
    /// Each (entity, action)'s string parameters that must not be empty and
    /// that the model gives no value. Decided when a dispatch carries the
    /// value, and only then.
    pub(in crate::composite::joint::model) nonempty: BTreeMap<(String, String), Vec<String>>,
    /// Each (entity, action)'s count parameters that must exceed a counter
    /// the model holds as a value, with that counter. Decided when the move
    /// carries the count, and only then.
    pub(in crate::composite::joint::model) greater:
        BTreeMap<(String, String), Vec<(String, String)>>,
}

impl Unchecked {
    /// Whether the model decides every constraint of `entity`'s `action`,
    /// sent as a dispatch or a request.
    pub(in crate::composite::joint::model) fn decides(&self, entity: &str, action: &str) -> bool {
        !self.taken.contains(pair!(entity, action))
    }

    /// Whether a sender choosing the parameters of `entity`'s `action` can
    /// meet its constraints, as far as the model knows.
    pub(in crate::composite::joint::model) fn meets(&self, entity: &str, action: &str) -> bool {
        !self.chosen.contains(pair!(entity, action))
    }

    /// The count parameters of `entity`'s `action` that must equal one of
    /// its counters, each with the counter.
    pub(in crate::composite::joint::model) fn counts(
        &self,
        entity: &str,
        action: &str,
    ) -> &[(String, String)] {
        self.counts
            .get(pair!(entity, action))
            .map_or(&[], Vec::as_slice)
    }

    /// The count parameters of `entity`'s `action` that must exceed one of
    /// its counters, each with the counter.
    pub(in crate::composite::joint::model) fn greater(
        &self,
        entity: &str,
        action: &str,
    ) -> &[(String, String)] {
        self.greater
            .get(pair!(entity, action))
            .map_or(&[], Vec::as_slice)
    }

    /// The string parameters of `entity`'s `action` the model gives no value
    /// that must not be empty.
    pub(in crate::composite::joint::model) fn nonempty(
        &self,
        entity: &str,
        action: &str,
    ) -> &[String] {
        self.nonempty
            .get(pair!(entity, action))
            .map_or(&[], Vec::as_slice)
    }

    /// The fields an `entity` record's module call keeps as it started:
    /// those the actions it may answer with (`answers`) tie a parameter to.
    pub(in crate::composite::joint::model) fn asked<'a>(
        &'a self,
        entity: &'a str,
        answers: impl Fn(&str) -> bool + 'a,
    ) -> impl Iterator<Item = &'a String> + 'a {
        self.equals
            .iter()
            .filter(move |((e, action), _)| e == entity && answers(action))
            .flat_map(|(_, fields)| fields.iter())
    }
}

/// What the model cannot decide of each action's constraints. Taking such
/// an action as allowed only adds behaviour for an invariant; for liveness it
/// is one of allowed and refused.
pub(in crate::composite::joint::model) fn unchecked_constraints(
    by_name: &BTreeMap<&str, &Automaton>,
    models: &BTreeMap<String, TemperModel>,
    order: &Order,
) -> Unchecked {
    let mut out = Unchecked::default();
    for (entity, model) in models {
        let tracked = |name: &str| model.tracked_strings.contains(name);
        let counted = |name: &str| {
            model.var_kinds.get(name) == Some(&VarKind::Counter)
                && !order.fields(entity).any(|(field, _)| field == name)
        };
        for action in &by_name[entity.as_str()].actions {
            let key = (entity.clone(), action.name.clone());
            let modelled = |param: &str| model.models_string(&action.name, param);
            let decided = |constraint: &ActionConstraint| match constraint {
                ActionConstraint::ParamGreaterThanField { param, field } => {
                    order.decides(entity, &action.name, param, field) || counted(field)
                }
                ActionConstraint::ParamEqualsField { field, .. } if counted(field) => true,
                ActionConstraint::ParamNonempty { .. } => true,
                _ => modelled(constraint.param()) && constraint.field().is_none_or(tracked),
            };
            if !action.constraints.iter().all(decided) {
                out.taken.insert(key.clone());
            }
            for constraint in &action.constraints {
                if let ActionConstraint::ParamGreaterThanField { param, field } = constraint
                    && counted(field)
                    && !order.decides(entity, &action.name, param, field)
                {
                    out.greater
                        .entry(key.clone())
                        .or_default()
                        .push((param.clone(), field.clone()));
                }
                if let ActionConstraint::ParamNonempty { param } = constraint
                    && !modelled(param)
                {
                    out.nonempty
                        .entry(key.clone())
                        .or_default()
                        .push(param.clone());
                }
                let ActionConstraint::ParamEqualsField { param, field } = constraint else {
                    continue;
                };
                if counted(field) {
                    out.counts
                        .entry(key.clone())
                        .or_default()
                        .push((param.clone(), field.clone()));
                }
                if tracked(field) {
                    out.equals
                        .entry(key.clone())
                        .or_default()
                        .insert(field.clone());
                } else {
                    out.chosen.insert(key.clone());
                }
            }
        }
    }
    out
}

impl Unchecked {
    /// These tables for every record's name.
    pub(in crate::composite::joint::model) fn fan(
        &mut self,
        copies: &BTreeMap<String, Vec<String>>,
    ) {
        use super::super::copies::{fan_moves, fan_set};
        fan_set(&mut self.taken, copies);
        fan_set(&mut self.chosen, copies);
        fan_moves(&mut self.equals, copies);
        fan_moves(&mut self.counts, copies);
        fan_moves(&mut self.nonempty, copies);
        fan_moves(&mut self.greater, copies);
    }
}
