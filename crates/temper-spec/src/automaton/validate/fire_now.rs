//! A state timer's `fire_now_on` names moves that make its armed timer fire
//! now. Each must be a move the record can make while in the timer's state
//! without leaving it, or it could never bring the timer forward: leaving the
//! state ends the timer. A move in `reset_on` re-arms the timer as its own
//! caller, which is what bringing it forward must not do, so a move may not be
//! in both.

use super::super::parser::AutomatonParseError;
use super::super::types::{ActionKind, Automaton, StateTimeout};

/// Refuse `st` when a move in its `fire_now_on` cannot bring its timer
/// forward from any state the record can be in.
pub(super) fn validate_fire_now_on(
    automaton: &Automaton,
    st: &StateTimeout,
) -> Result<(), AutomatonParseError> {
    let refuse = |why: String| {
        Err(AutomatonParseError::Validation(format!(
            "state_timeout for '{}': fire_now_on {why}",
            st.state
        )))
    };
    for name in &st.fire_now_on {
        let Some(action) = automaton.actions.iter().find(|a| &a.name == name) else {
            return refuse(format!("references unknown action '{name}'"));
        };
        if name == &st.on_timeout {
            return refuse(format!(
                "names '{name}', the timer's own on_timeout, which cannot bring itself forward"
            ));
        }
        if st.reset_on.contains(name) {
            return refuse(format!(
                "names '{name}', which is also in reset_on: a reset re-arms the timer as \
                 the move's caller, and bringing it forward must keep whoever armed it"
            ));
        }
        if action.kind == ActionKind::Output {
            return refuse(format!(
                "names '{name}', an output action, which a record never takes"
            ));
        }
        let starts_there = if action.from.is_empty() {
            action.kind.enabled_everywhere()
        } else {
            action.from.contains(&st.state)
        };
        let stays = action.to.as_ref().is_none_or(|to| to == &st.state);
        if !(starts_there && stays) {
            return refuse(format!(
                "names '{name}', which no record in '{}' can take and stay there, so it \
                 never finds the timer armed",
                st.state
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::automaton::parser::{LivenessEnforcement, parse_automaton_with_liveness};

    /// A change waiting on GitHub: a person's `Watch` enters `Watching`, whose
    /// timer `Look`s after a day. A day is the deadline for a delivery GitHub
    /// lost, not a poll: GitHub does not redeliver a failed delivery by
    /// itself. `timer` adds to the timer's declaration.
    fn spec(timer: &str) -> String {
        format!(
            r#"
[automaton]
name = "Change"
states = ["Waiting", "Watching", "Done"]
initial = "Waiting"

[[action]]
name = "Watch"
kind = "input"
from = ["Waiting"]
to = "Watching"

[[action]]
name = "Heard"
kind = "input"
from = ["Watching"]

[[action]]
name = "Leave"
kind = "input"
from = ["Watching"]
to = "Done"

[[action]]
name = "Elsewhere"
kind = "input"
from = ["Waiting"]

[[action]]
name = "Look"
kind = "internal"
from = ["Watching"]

[[state_timeout]]
state = "Watching"
after_seconds = 86400
on_timeout = "Look"
{timer}
"#
        )
    }

    fn parse(source: &str) -> Result<(), String> {
        parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[track_caller]
    fn refused(timer: &str, needle: &str) {
        match parse(&spec(timer)) {
            Ok(()) => panic!("loaded a timer that should be refused with `{needle}`: {timer}"),
            Err(error) => assert!(error.contains(needle), "`{needle}` not in: {error}"),
        }
    }

    #[test]
    fn a_move_that_stays_in_the_timers_state_loads() {
        parse(&spec(r#"fire_now_on = ["Heard"]"#)).expect("loads");
    }

    #[test]
    fn a_move_no_record_in_the_state_can_take_and_stay_is_refused() {
        // Leaving the state ends the timer.
        refused(r#"fire_now_on = ["Leave"]"#, "can take and stay there");
        // Never taken in the state at all.
        refused(r#"fire_now_on = ["Elsewhere"]"#, "can take and stay there");
        refused(r#"fire_now_on = ["Nobody"]"#, "unknown action 'Nobody'");
    }

    #[test]
    fn a_move_that_resets_or_is_the_timer_is_refused() {
        refused(
            "reset_on = [\"Heard\"]\nfire_now_on = [\"Heard\"]",
            "also in reset_on",
        );
        refused(r#"fire_now_on = ["Look"]"#, "own on_timeout");
    }
}
