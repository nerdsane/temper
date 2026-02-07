//! Orchestrate a multi-level verification cascade.
//!
//! The cascade runs verification in stages:
//!   1. Model checking (exhaustive state-space exploration via Stateright)
//!
//! Each level produces a pass/fail result. The cascade stops at the first
//! failure or runs through all levels if everything passes.

use crate::checker::{self, VerificationResult};
use crate::model::{self, TemperModel};

/// The levels available in the verification cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeLevel {
    /// Exhaustive model checking via Stateright.
    ModelCheck,
}

impl std::fmt::Display for CascadeLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CascadeLevel::ModelCheck => write!(f, "Model Check"),
        }
    }
}

/// The result of a single cascade level.
#[derive(Debug, Clone)]
pub struct LevelResult {
    /// Which level produced this result.
    pub level: CascadeLevel,
    /// Whether this level passed.
    pub passed: bool,
    /// A human-readable summary of the result.
    pub summary: String,
    /// The detailed verification result (for model checking).
    pub verification: Option<VerificationResult>,
}

/// The aggregate result of running the full verification cascade.
#[derive(Debug, Clone)]
pub struct CascadeResult {
    /// Whether all levels passed.
    pub all_passed: bool,
    /// Per-level results.
    pub levels: Vec<LevelResult>,
}

impl CascadeResult {
    /// Return the result for a specific level, if it was run.
    pub fn level_result(&self, level: CascadeLevel) -> Option<&LevelResult> {
        self.levels.iter().find(|r| r.level == level)
    }
}

/// Orchestrates the verification cascade for a TLA+ state machine.
///
/// Builds the Stateright model from the spec, runs model checking, and
/// collects results.
pub struct VerificationCascade {
    /// Raw TLA+ source (if provided, enables guard resolution).
    tla_source: Option<String>,
    /// Pre-parsed state machine (used when TLA+ source is not available).
    state_machine: Option<temper_spec::tlaplus::StateMachine>,
    /// Maximum items for bounded model checking.
    max_items: usize,
}

impl VerificationCascade {
    /// Create a new cascade from a pre-parsed `StateMachine`.
    pub fn new(state_machine: temper_spec::tlaplus::StateMachine) -> Self {
        Self {
            tla_source: None,
            state_machine: Some(state_machine),
            max_items: 2,
        }
    }

    /// Create a new cascade from raw TLA+ source, which enables full guard
    /// resolution for `CanXxx` predicates.
    pub fn from_tla(tla_source: &str) -> Self {
        Self {
            tla_source: Some(tla_source.to_string()),
            state_machine: None,
            max_items: 2,
        }
    }

    /// Set the maximum item count for bounded model checking.
    pub fn with_max_items(mut self, max_items: usize) -> Self {
        self.max_items = max_items;
        self
    }

    /// Run the full verification cascade.
    pub fn run(&self) -> CascadeResult {
        let mut levels = Vec::new();

        let temper_model = self.build_temper_model();
        let model_check_result = self.run_model_check(&temper_model);
        let all_passed = model_check_result.passed;
        levels.push(model_check_result);

        CascadeResult { all_passed, levels }
    }

    /// Build the TemperModel, using TLA+ source if available.
    fn build_temper_model(&self) -> TemperModel {
        if let Some(ref source) = self.tla_source {
            model::build_model_from_tla(source, self.max_items)
        } else if let Some(ref sm) = self.state_machine {
            model::build_model_with_max_items(sm, self.max_items)
        } else {
            panic!("VerificationCascade requires either TLA+ source or a StateMachine");
        }
    }

    /// Run the model checking level.
    fn run_model_check(&self, model: &TemperModel) -> LevelResult {
        let verification = checker::check_model(model);

        let passed = verification.all_properties_hold;
        let summary = if passed {
            format!(
                "Model check PASSED: {} states explored, all properties hold",
                verification.states_explored,
            )
        } else {
            format!(
                "Model check FAILED: {} states explored, {} counterexample(s) found",
                verification.states_explored,
                verification.counterexamples.len(),
            )
        };

        LevelResult {
            level: CascadeLevel::ModelCheck,
            passed,
            summary,
            verification: Some(verification),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDER_TLA: &str = include_str!("../../../reference/ecommerce/specs/order.tla");

    #[test]
    fn test_cascade_passes_for_valid_spec() {
        let cascade = VerificationCascade::from_tla(ORDER_TLA);
        let result = cascade.run();
        assert!(result.all_passed, "cascade should pass for the reference order spec");
        assert_eq!(result.levels.len(), 1);
    }

    #[test]
    fn test_cascade_model_check_level_result() {
        let cascade = VerificationCascade::from_tla(ORDER_TLA);
        let result = cascade.run();

        let mc = result.level_result(CascadeLevel::ModelCheck).expect("should have model check result");
        assert!(mc.passed);
        assert!(mc.summary.contains("PASSED"));
        assert!(mc.verification.is_some());
        let v = mc.verification.as_ref().unwrap();
        assert!(v.states_explored > 0);
    }

    #[test]
    fn test_cascade_with_custom_max_items() {
        let cascade = VerificationCascade::from_tla(ORDER_TLA).with_max_items(1);
        let result = cascade.run();
        assert!(result.all_passed);
    }
}
