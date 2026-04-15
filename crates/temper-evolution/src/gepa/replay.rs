//! Trajectory replay against candidate specs.
//!
//! Replays recorded OTS trajectory actions against a candidate
//! TransitionTable to measure how well the candidate handles
//! the same workload.

use serde::{Deserialize, Serialize};

/// Result of replaying a trajectory against a candidate spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayResult {
    /// Total number of actions attempted during replay.
    pub actions_attempted: u32,

    /// Number of actions that succeeded (valid transition).
    pub succeeded: u32,

    /// Number of actions rejected by guards.
    pub guard_rejections: u32,

    /// Number of actions not found in the spec.
    pub unknown_actions: u32,

    /// Number of invalid transitions (action exists but not from current state).
    pub invalid_transitions: u32,

    /// Detailed error messages for failed actions.
    pub errors: Vec<ReplayError>,
}

/// A single replay error with context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayError {
    /// The action that was attempted.
    pub action: String,

    /// The entity state at the time of the attempt.
    pub from_state: String,

    /// What went wrong.
    pub error_kind: ReplayErrorKind,

    /// Detailed message.
    pub message: String,
}

/// Classification of replay errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayErrorKind {
    /// Action not defined in the spec.
    UnknownAction,
    /// Guard condition not satisfied.
    GuardRejection,
    /// Transition not valid from current state.
    InvalidTransition,
    /// Spec evaluation error.
    EvaluationError,
}

impl ReplayResult {
    /// Create a new empty replay result.
    pub fn new() -> Self {
        Self {
            actions_attempted: 0,
            succeeded: 0,
            guard_rejections: 0,
            unknown_actions: 0,
            invalid_transitions: 0,
            errors: Vec::new(),
        }
    }

    /// Record a successful action.
    pub fn record_success(&mut self) {
        self.actions_attempted += 1;
        self.succeeded += 1;
    }

    /// Record a guard rejection.
    pub fn record_guard_rejection(&mut self, action: &str, from_state: &str, message: String) {
        self.actions_attempted += 1;
        self.guard_rejections += 1;
        self.errors.push(ReplayError {
            action: action.into(),
            from_state: from_state.into(),
            error_kind: ReplayErrorKind::GuardRejection,
            message,
        });
    }

    /// Record an unknown action.
    pub fn record_unknown_action(&mut self, action: &str, from_state: &str) {
        self.actions_attempted += 1;
        self.unknown_actions += 1;
        self.errors.push(ReplayError {
            action: action.into(),
            from_state: from_state.into(),
            error_kind: ReplayErrorKind::UnknownAction,
            message: format!("Action '{}' not defined in spec", action),
        });
    }

    /// Record an invalid transition.
    pub fn record_invalid_transition(&mut self, action: &str, from_state: &str, message: String) {
        self.actions_attempted += 1;
        self.invalid_transitions += 1;
        self.errors.push(ReplayError {
            action: action.into(),
            from_state: from_state.into(),
            error_kind: ReplayErrorKind::InvalidTransition,
            message,
        });
    }

    /// Check if the replay was fully successful.
    pub fn all_succeeded(&self) -> bool {
        self.actions_attempted > 0 && self.succeeded == self.actions_attempted
    }

    /// Success rate as a fraction (0.0 to 1.0).
    pub fn success_rate(&self) -> f64 {
        if self.actions_attempted == 0 {
            return 0.0;
        }
        self.succeeded as f64 / self.actions_attempted as f64
    }
}

impl Default for ReplayResult {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replay_result_tracking() {
        let mut result = ReplayResult::new();

        result.record_success();
        result.record_success();
        result.record_guard_rejection("Reassign", "Open", "guard failed".into());
        result.record_unknown_action("Archive", "Open");

        assert_eq!(result.actions_attempted, 4);
        assert_eq!(result.succeeded, 2);
        assert_eq!(result.guard_rejections, 1);
        assert_eq!(result.unknown_actions, 1);
        assert_eq!(result.errors.len(), 2);
        assert!(!result.all_succeeded());
        assert!((result.success_rate() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_replay_result_perfect() {
        let mut result = ReplayResult::new();
        result.record_success();
        result.record_success();
        result.record_success();

        assert!(result.all_succeeded());
        assert!((result.success_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_replay_result_empty() {
        let result = ReplayResult::new();
        assert!(!result.all_succeeded());
        assert!((result.success_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_replay_error_serialization() {
        let error = ReplayError {
            action: "Reassign".into(),
            from_state: "Open".into(),
            error_kind: ReplayErrorKind::UnknownAction,
            message: "not defined".into(),
        };

        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("\"unknown_action\""));

        let parsed: ReplayError = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.error_kind, ReplayErrorKind::UnknownAction);
    }
}
