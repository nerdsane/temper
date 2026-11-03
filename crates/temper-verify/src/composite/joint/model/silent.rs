//! Moves a proof's walk leaves out, and string fields nothing reads.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Arg, Expr, Name, VarKind};

use super::JointModel;
use super::read::reactions_into;

impl JointModel {
    /// This model without its silent moves, and without the kinds of string
    /// fields it does not track and nothing names.
    ///
    /// A move is silent when it stays in its place, has no guard, takes no
    /// parameter it writes and has no constraint, writes nothing the model
    /// holds, is its entity's only transition of that name, and the kernel
    /// does nothing on it: no reaction or module call starts from it, no
    /// reaction, timer or module answer sends it, it resets no state timer,
    /// no module open on its entity may answer with it, and no liveness
    /// relies on it. Taking one leaves the joint state as it was, so the
    /// walk reaches the same states with or without it, and a state reaches
    /// the same others: the invariants and liveness find the same.
    pub(in crate::composite::joint) fn without_silent(
        mut self,
        by_name: &BTreeMap<&str, &Automaton>,
    ) -> Self {
        let silent = self.silent(by_name);
        for (entity, model) in &mut self.models {
            let mut kept = Vec::new();
            for (index, t) in model.transitions.iter().enumerate() {
                if !silent.contains(&(entity.clone(), t.name.clone())) {
                    kept.push(index);
                }
            }
            if let Some(ways) = self.unsent.get_mut(entity) {
                for index in ways.iter_mut() {
                    *index = kept
                        .iter()
                        .position(|old| old == index)
                        .expect("a move nothing sends is not silent");
                }
            }
            model
                .transitions
                .retain(|t| !silent.contains(&(entity.clone(), t.name.clone())));
        }
        self.senders.forget(&silent);
        let named = self.named();
        for (entity, model) in &mut self.models {
            let tracked = model.tracked_strings.clone();
            model.var_kinds.retain(|field, kind| {
                *kind != VarKind::Str
                    || tracked.contains(field)
                    || named.contains(&(entity.clone(), field.clone()))
            });
        }
        self
    }

    /// The silent moves, as (entity, action).
    fn silent(&self, by_name: &BTreeMap<&str, &Automaton>) -> BTreeSet<(String, String)> {
        let scope: BTreeSet<String> = self.models.keys().cloned().collect();
        let into = reactions_into(by_name, &scope);
        let mut silent = BTreeSet::new();
        for (entity, model) in &self.models {
            let open = self.kernel.calls_on.iter().any(|((e, _), specs)| {
                e == entity && specs.iter().any(|s| s.on_success.is_empty())
            });
            for t in &model.transitions {
                let key = (entity.clone(), t.name.clone());
                let Some(action) = by_name[entity.as_str()]
                    .actions
                    .iter()
                    .find(|a| a.name == t.name)
                else {
                    continue;
                };
                let stays = t.to_state.is_none()
                    || t.from_states
                        .iter()
                        .all(|from| Some(from) == t.to_state.as_ref());
                let quiet = !t.from_states.is_empty()
                    && stays
                    && t.guard == Expr::Const(true)
                    && t.effects.is_empty()
                    && t.params.is_empty()
                    && t.string_params.iter().all(|p| !p.writes)
                    && action.triggers.is_empty()
                    && action.constraints.is_empty()
                    && model
                        .transitions
                        .iter()
                        .filter(|o| o.name == t.name)
                        .count()
                        == 1;
                let unsent = !open
                    && !into.contains_key(&key)
                    && !self.relies_on.contains(&key)
                    && !self.kernel.chooses(entity, &t.name)
                    && !self.kernel.calls_on.contains_key(&key)
                    && !self.kernel.timers.get(entity).is_some_and(|timers| {
                        timers.iter().any(|timer| timer.reset_on.contains(&t.name))
                    })
                    && !self.reactions.iter().any(|r| {
                        (&r.from == entity && r.source_action == t.name)
                            || (&r.to == entity && r.target_action == t.name)
                    });
                if quiet && unsent {
                    silent.insert(key);
                }
            }
        }
        silent
    }

    /// The fields some guard, reaction or constraint names, as (entity,
    /// field).
    fn named(&self) -> BTreeSet<(String, String)> {
        let mut named = BTreeSet::new();
        let mut name = |entity: &str, n: Name<'_>| match n {
            Name::Var(field) => {
                named.insert((entity.to_string(), field.to_string()));
            }
            Name::CrossStatus { id_field, .. } => {
                named.insert((entity.to_string(), id_field.to_string()));
            }
            Name::CrossField {
                entity_type,
                id_field,
                field,
            } => {
                named.insert((entity.to_string(), id_field.to_string()));
                named.insert((entity_type.to_string(), field.to_string()));
            }
            Name::Status | Name::Param(_) => {}
        };
        for (entity, model) in &self.models {
            for t in &model.transitions {
                t.guard.for_each_name(&mut |n| name(entity, n));
                for p in &t.string_params {
                    for field in p.equals.iter().chain(&p.not_equals) {
                        name(entity, Name::Var(field));
                    }
                }
            }
        }
        for r in &self.reactions {
            if let Some(guard) = &r.guard {
                guard.for_each_name(&mut |n| name(&r.from, n));
            }
            for arg in r.args.values() {
                if let Arg::Var(field) = arg {
                    name(&r.from, Name::Var(field));
                }
            }
        }
        named
    }
}
