//! Same-type trigger and timer argument contracts, checked at install only.

use super::LintFinding;
use crate::automaton::{Automaton, TriggerKind};

/// Find undeclared arguments passed to declared actions on this strict type.
///
/// Installs and upgrades refuse these errors before writing. Registry loading
/// only logs them, so boot can restore specs installed before this check existed. Targets
/// on other types need the app's full bundle and remain a follow-up.
pub fn lint_trigger_target_args(automaton: &Automaton, registered_type: &str) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    for action in &automaton.actions {
        for trigger in &action.triggers {
            // Directory loaders may derive a different name from the filename.
            // The registered name determines whether the target is local.
            if trigger.kind == TriggerKind::Entity
                && trigger.target_entity.as_deref() == Some(registered_type)
                && let Some(target) = trigger.target_action.as_deref()
            {
                check_target_accepts(
                    automaton,
                    &format!("trigger '{}' on action '{}'", trigger.name, action.name),
                    target,
                    trigger.args.keys(),
                    &mut findings,
                );
            }
        }
    }
    for timer in &automaton.state_timeouts {
        check_target_accepts(
            automaton,
            &format!("state_timeout for '{}'", timer.state),
            &timer.on_timeout,
            timer.args.keys(),
            &mut findings,
        );
    }
    findings
}

/// On a `strict_action_params` type the kernel refuses a move carrying a
/// parameter its action does not declare. A trigger or timer on this same type
/// that passes one fails every time it fires, so its install is refused here.
/// Undeclared targets (carry moves) and non-strict types are left to the kernel.
fn check_target_accepts<'a>(
    automaton: &Automaton,
    scope: &str,
    target: &str,
    keys: impl IntoIterator<Item = &'a String>,
    findings: &mut Vec<LintFinding>,
) {
    if !automaton.automaton.strict_action_params {
        return;
    }
    let Some(action) = automaton.actions.iter().find(|a| a.name == target) else {
        return;
    };
    for key in keys {
        if !action.params.iter().any(|p| p.name() == key) {
            findings.push(LintFinding::error(
                "trigger_target_arg_undeclared",
                format!(
                    "{scope} passes '{key}' to action '{target}', which does not declare it: \
                 this type has strict_action_params, so every dispatch would be refused \
                 (\"Action '{target}' does not accept parameter '{key}'\"). Declare '{key}' \
                 in '{target}' params, or drop it from args"
                ),
            ));
        }
    }
}

#[cfg(test)]
#[path = "trigger_args_test.rs"]
mod tests;
