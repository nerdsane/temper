//! The counters, booleans and lists the model holds.

use std::collections::BTreeSet;

use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Arg, Effect, Name};

use super::rule_asserts;

/// The fields the model holds a value for: those a guard, an
/// `[[invariant]]` or an `[[entry_rule]]` reads, a constraint ties a
/// parameter to, or `also_read` names (another record's rule reads them),
/// and every field an effect copies into one of those. The model leaves the
/// others out, with the effects that write them: no check reads their value,
/// so holding one only multiplies the states. A counter one move bumped and
/// nothing read took a spec from 137,137 states to 411,411.
///
/// Strings are chosen separately ([`super::strings::tracked_strings`]); this
/// set decides counters, booleans and lists.
pub(super) fn read_fields(automaton: &Automaton, also_read: &BTreeSet<String>) -> BTreeSet<String> {
    let mut read = also_read.clone();
    let guards = automaton.actions.iter().map(|action| &action.guard);
    for expr in guards.chain(rule_asserts(automaton)) {
        expr.for_each_name(&mut |name| {
            if let Name::Var(var) = name {
                read.insert(var.to_string());
            }
        });
    }
    for action in &automaton.actions {
        read.extend(
            action
                .constraints
                .iter()
                .filter_map(|c| c.field())
                .map(str::to_string),
        );
    }
    // A copy into a field the model holds needs the value copied.
    let copies: Vec<(&str, &str)> = automaton
        .actions
        .iter()
        .flat_map(|action| &action.effect)
        .filter_map(|effect| match effect {
            Effect::Assign {
                var,
                value: Arg::Var(source),
                ..
            }
            | Effect::Append {
                list: var,
                value: Arg::Var(source),
            }
            | Effect::RemoveAt {
                list: var,
                index: Arg::Var(source),
            } => Some((var.as_str(), source.as_str())),
            _ => None,
        })
        .collect();
    loop {
        let before = read.len();
        for (target, source) in &copies {
            if read.contains(*target) {
                read.insert(source.to_string());
            }
        }
        if read.len() == before {
            return read;
        }
    }
}
