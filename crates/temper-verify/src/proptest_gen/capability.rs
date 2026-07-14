use super::{PropTestFailure, PropTestResult};
use crate::model::{InvariantKind, TemperModel, TemperModelState};

pub(super) fn failure(
    model: &TemperModel,
    initial_state: &TemperModelState,
) -> Option<PropTestResult> {
    model.invariants.iter().find_map(|invariant| {
        matches!(invariant.kind, InvariantKind::Unverifiable { .. }).then(|| PropTestResult {
            total_cases: 0,
            passed: false,
            failure: Some(PropTestFailure {
                invariant: invariant.name.clone(),
                action_sequence: Vec::new(),
                final_state: format!("{initial_state}"),
            }),
        })
    })
}
