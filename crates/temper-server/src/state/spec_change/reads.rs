//! What the check reads: the states whose records a change can affect, and
//! the names whose values group those records.

use std::collections::BTreeSet;

use temper_jit::table::{Effect, TransitionTable};
use temper_spec::automaton::{Carry, is_carry_move};
use temper_spec::predicate::{Arg, Name};

/// The carry moves of a version.
pub(super) fn carry_moves(table: &TransitionTable) -> BTreeSet<&str> {
    table
        .rules
        .iter()
        .filter(|rule| is_carry_move(&rule.name))
        .map(|rule| rule.name.as_str())
        .collect()
}

/// The states whose records a change can leave with no way forward, or a
/// carry moves: those `new` drops, those a move whose conditions changed
/// starts from under either version, and those a carry of either version
/// starts from, since records may still wait there for the installed one's.
pub(super) fn affected_states(
    old: &TransitionTable,
    new: &TransitionTable,
    carries: &[(String, &Carry)],
) -> BTreeSet<String> {
    let mut states: BTreeSet<String> = old
        .states
        .iter()
        .filter(|state| !new.states.contains(state))
        .cloned()
        .collect();
    let moves: BTreeSet<&str> = old
        .caller_moves()
        .into_iter()
        .chain(new.caller_moves())
        .collect();
    for action in moves {
        if old.move_conditions(action) == new.move_conditions(action) {
            continue;
        }
        for rule in old.rules.iter().chain(&new.rules) {
            if rule.name != action {
                continue;
            }
            if rule.from_states.is_empty() {
                states.extend(old.states.iter().cloned());
            } else {
                states.extend(rule.from_states.iter().cloned());
            }
        }
    }
    states.extend(carries.iter().map(|(_, carry)| carry.from.clone()));
    for rule in &old.rules {
        if is_carry_move(&rule.name) {
            states.extend(rule.from_states.iter().cloned());
        }
    }
    states
}

/// The names the check reads from a record, which group the stored records:
/// what the moves of either version and the installed version's carries
/// read, and under the new version what each carry's guard and effects and
/// its `then` move's effects read, the field its `as` names, which the then
/// move's Cedar sees as `context.actingFor`, what its invariants read, and
/// the fields naming the related records its moves and invariants read.
/// Records in one state that agree on all of them end the same way in a
/// carry's dry run, so the dry run reads one record per group.
pub(super) fn read_names(
    old: &TransitionTable,
    new: &TransitionTable,
    carries: &[(String, &Carry)],
) -> Vec<String> {
    let mut names = BTreeSet::new();
    for table in [old, new] {
        for action in table.caller_moves() {
            names.extend(table.move_reads(action));
        }
    }
    for action in carry_moves(old) {
        names.extend(old.move_reads(action));
    }
    let mut read = |name: Name<'_>| {
        if let Name::Var(var) = name {
            names.insert(var.to_string());
        }
    };
    for (_, carry) in carries {
        carry.guard.for_each_name(&mut read);
        if let Some(field) = &carry.acts_for {
            read(Name::Var(field));
        }
    }
    for invariant in &new.invariants {
        invariant.assert.for_each_name(&mut read);
    }
    let made = carries
        .iter()
        .flat_map(|(action, carry)| std::iter::once(action.as_str()).chain(carry.then.as_deref()));
    for action in made {
        names.extend(new.move_reads(action));
        let effects = new
            .rules
            .iter()
            .filter(|rule| rule.name == action)
            .flat_map(|rule| &rule.effects);
        for effect in effects {
            let (var, value) = match effect {
                Effect::SetCounter { var, value }
                | Effect::AddCounter { var, value }
                | Effect::SubCounter { var, value }
                | Effect::SetBool { var, value }
                | Effect::ListAppend { var, value }
                | Effect::ListRemove { var, value } => (var, value),
                Effect::ListRemoveAt { var, index } => (var, index),
                _ => continue,
            };
            names.insert(var.clone());
            if let Arg::Var(read) = value {
                names.insert(read.clone());
            }
        }
    }
    let actions = carries
        .iter()
        .map(|(action, _)| action.as_str())
        .chain(new.caller_moves());
    for action in actions {
        names.extend(new.related_refs(action).into_iter().map(|(_, field)| field));
    }
    names.into_iter().collect()
}

/// Whether a guard of `action` reads a related record.
pub(super) fn reads_related(table: &TransitionTable, action: &str) -> bool {
    table
        .rules
        .iter()
        .any(|rule| rule.name == action && !rule.guard.related_types().is_empty())
}
