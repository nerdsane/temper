use super::types::*;

mod properties;
mod source;
mod states;
mod transitions;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum TlaExtractError {
    #[error("no MODULE declaration found")]
    NoModule,
    #[error("no state set found (expected a set assignment like States == {{...}})")]
    NoStates,
    #[error("parse error: {0}")]
    Parse(String),
}

/// Extract state machine structure from a TLA+ specification.
///
/// This is a pragmatic extractor, not a full TLA+ parser. It uses pattern
/// matching to find:
/// - MODULE name
/// - CONSTANTS and VARIABLES
/// - State set definitions (OrderStatuses == {...})
/// - Action definitions (Name == /\ guard /\ effect)
/// - Invariants (safety properties)
/// - Liveness properties (temporal formulas with ~>)
pub fn extract_state_machine(tla_source: &str) -> Result<StateMachine, TlaExtractError> {
    let module_name = source::extract_module_name(tla_source)?;
    let constants = source::extract_list_after(tla_source, "CONSTANTS");
    let variables = source::extract_list_after(tla_source, "VARIABLES");
    let states = states::extract_states(tla_source)?;
    let transitions = transitions::extract_transitions(tla_source, &states);
    let invariants = properties::extract_invariants(tla_source);
    let liveness_properties = properties::extract_liveness(tla_source);

    Ok(StateMachine {
        module_name,
        states,
        transitions,
        invariants,
        liveness_properties,
        constants,
        variables,
    })
}
