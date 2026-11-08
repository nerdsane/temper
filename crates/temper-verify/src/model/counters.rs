//! Counters nothing reads are left out of one entity's model, as strings
//! nothing reads already are. The joint check slices its own models
//! (`composite::joint::slice`).

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::{Arg, Name, VarKind};

use super::builder::effect_params;
use super::types::{ModelEffect, TemperModel};
use temper_spec::automaton::Automaton;

/// Drop from `model` every counter no guard, invariant or entry rule reads,
/// unless an effect copies it into a variable the model keeps, with the effects that
/// write it and the parameters only they used. Such a counter cannot change
/// whether a move may fire or a rule holds, so leaving it out only merges
/// states that differ in it: a pass is no longer bounded by a count no rule
/// looks at, and a move no longer tries values of a parameter only that
/// count takes.
pub(super) fn leave_out_unread_counters(automaton: &Automaton, model: &mut TemperModel) {
    let counters: BTreeSet<&str> = automaton
        .state
        .iter()
        .filter(|sv| sv.var_type.kind() == VarKind::Counter)
        .map(|sv| sv.name.as_str())
        .collect();
    let mut read = BTreeSet::new();
    let guards = model.transitions.iter().map(|t| &t.guard);
    let invariants = model.invariants.iter().map(|inv| &inv.assert);
    let entry_rules = model.entry_rules.iter().map(|rule| &rule.assert);
    for expr in guards.chain(invariants).chain(entry_rules) {
        expr.for_each_name(&mut |name| {
            if let Name::Var(var) = name {
                read.insert(var.to_string());
            }
        });
    }
    // Every variable but a counter is kept, so what an effect on one reads
    // is read; a counter's effects count once the counter is read.
    loop {
        let before = read.len();
        for effect in model.transitions.iter().flat_map(|t| &t.effects) {
            let (var, value) = match effect {
                ModelEffect::SetCounter { var, value }
                | ModelEffect::AddCounter { var, value }
                | ModelEffect::SubCounter { var, value }
                | ModelEffect::SetBool { var, value }
                | ModelEffect::ListAppend { var, value }
                | ModelEffect::ListRemove { var, value } => (var, value),
                ModelEffect::ListRemoveAt { var, index } => (var, index),
                ModelEffect::SetString { .. } => continue,
            };
            if (read.contains(var.as_str()) || !counters.contains(var.as_str()))
                && let Arg::Var(source) = value
            {
                read.insert(source.clone());
            }
        }
        if read.len() == before {
            break;
        }
    }
    let unread = |name: &str| counters.contains(name) && !read.contains(name);
    model.initial_counters.retain(|name, _| !unread(name));
    model.counter_bounds.retain(|name, _| !unread(name));
    for t in &mut model.transitions {
        t.effects.retain(|effect| match effect {
            ModelEffect::SetCounter { var, .. }
            | ModelEffect::AddCounter { var, .. }
            | ModelEffect::SubCounter { var, .. } => !unread(var),
            ModelEffect::SetBool { .. }
            | ModelEffect::SetString { .. }
            | ModelEffect::ListAppend { .. }
            | ModelEffect::ListRemove { .. }
            | ModelEffect::ListRemoveAt { .. } => true,
        });
        let effect_params = effect_params(&t.effects);
        // Entry rules choose guard and constraint inputs too, even when
        // their only effect writes a counter that was left out.
        let guard_params = t.guard.params_read();
        let kept = |name: &String| {
            effect_params.contains_key(name)
                || (!model.entry_rules.is_empty()
                    && (guard_params.contains(name)
                        || t.string_params.iter().any(|param| &param.name == name)))
        };
        // What follows the move still reads the values it carried.
        let after = t.after.params_read();
        let (params, left): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut t.params)
            .into_iter()
            .partition(|(name, _)| kept(name));
        t.params = params;
        t.after_params
            .extend(left.into_iter().filter(|(name, _)| after.contains(name)));
    }
}
