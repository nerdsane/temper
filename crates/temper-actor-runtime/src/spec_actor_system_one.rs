//! Explicit refusal of unsupported inference guards on the Postgres adapter.

use temper_jit::table::{Guard, TransitionTable};
use temper_spec::automaton::Automaton;

use crate::actor::ActorError;

pub(super) fn reject_declarations(automaton: &Automaton) -> Result<(), String> {
    if automaton.actions.iter().any(|action| {
        action
            .guard
            .iter()
            .any(|guard| matches!(guard, temper_spec::automaton::Guard::SystemOne(_)))
    }) {
        return Err("system_one guards are not supported by the Postgres actor runtime".into());
    }
    Ok(())
}

pub(super) fn reject_action(table: &TransitionTable, action: &str) -> Result<(), ActorError> {
    fn contains(guard: &Guard) -> bool {
        match guard {
            Guard::SystemOne(_) => true,
            Guard::And(parts) => parts.iter().any(contains),
            _ => false,
        }
    }
    if table
        .rules
        .iter()
        .any(|rule| rule.name == action && contains(&rule.guard))
    {
        return Err(ActorError::Rejected(
            "system_one guards are not supported by the Postgres actor runtime".into(),
        ));
    }
    Ok(())
}
