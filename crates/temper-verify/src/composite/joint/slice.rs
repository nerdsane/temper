//! Slicing one entity's model to what the joint invariants depend on.

use std::collections::BTreeSet;

use temper_spec::predicate::{Arg, Name, VarKind};

use crate::model::builder::effect_params;
use crate::model::{ModelEffect, TemperModel};

/// `model` with only the counters, booleans and lists `read` names, or that
/// can change one that is kept: a transition that writes a kept variable
/// keeps what its guard and effects read. Everything else is dropped, so a
/// guard that read it is a free choice and its effects are gone. Tracked
/// strings and the status are always kept.
pub(super) fn slice(mut model: TemperModel, read: BTreeSet<String>) -> TemperModel {
    let mut keep: BTreeSet<String> = read;
    keep.extend(model.tracked_strings.iter().cloned());
    loop {
        let before = keep.len();
        for t in &model.transitions {
            let writes = t.effects.iter().any(|e| keep.contains(effect_var(e)))
                || t.string_params
                    .iter()
                    .any(|p| p.writes && keep.contains(&p.name));
            if !writes {
                continue;
            }
            t.guard.for_each_name(&mut |n| {
                if let Name::Var(var) = n {
                    keep.insert(var.to_string());
                }
            });
            for effect in &t.effects {
                if let Some(var) = effect_reads(effect) {
                    keep.insert(var.to_string());
                }
            }
        }
        if keep.len() == before {
            break;
        }
    }
    let modeled = |kind: &VarKind| matches!(kind, VarKind::Counter | VarKind::Bool | VarKind::List);
    let dropped: BTreeSet<String> = model
        .var_kinds
        .iter()
        .filter(|(name, kind)| modeled(kind) && !keep.contains(*name))
        .map(|(name, _)| name.clone())
        .collect();
    for name in &dropped {
        model.var_kinds.remove(name);
        model.initial_counters.remove(name);
        model.initial_booleans.remove(name);
        model.initial_lists.remove(name);
        model.counter_bounds.remove(name);
        model.list_bounds.remove(name);
        model.list_literals.remove(name);
    }
    model.invariants.clear();
    model.entry_rules.clear();
    model.liveness.clear();
    for t in &mut model.transitions {
        t.effects.retain(|e| !dropped.contains(effect_var(e)));
        // What follows the move still reads the values it carried.
        let effect = effect_params(&t.effects);
        let after = t.after.params_read();
        for (name, kind) in &t.params {
            if !effect.contains_key(name) && after.contains(name) {
                t.after_params.insert(name.clone(), *kind);
            }
        }
        t.params = effect;
    }
    model
}

fn effect_var(effect: &ModelEffect) -> &str {
    match effect {
        ModelEffect::SetCounter { var, .. }
        | ModelEffect::AddCounter { var, .. }
        | ModelEffect::SubCounter { var, .. }
        | ModelEffect::SetBool { var, .. }
        | ModelEffect::SetString { var, .. }
        | ModelEffect::ListAppend { var, .. }
        | ModelEffect::ListRemove { var, .. }
        | ModelEffect::ListRemoveAt { var, .. } => var,
    }
}

/// The state variable read by an effect argument, if any.
pub(super) fn effect_reads(effect: &ModelEffect) -> Option<&str> {
    let arg = match effect {
        ModelEffect::SetCounter { value, .. }
        | ModelEffect::AddCounter { value, .. }
        | ModelEffect::SubCounter { value, .. }
        | ModelEffect::SetBool { value, .. }
        | ModelEffect::ListAppend { value, .. }
        | ModelEffect::ListRemove { value, .. } => value,
        ModelEffect::ListRemoveAt { index, .. } => index,
        ModelEffect::SetString { .. } => return None,
    };
    match arg {
        Arg::Var(var) => Some(var),
        _ => None,
    }
}
