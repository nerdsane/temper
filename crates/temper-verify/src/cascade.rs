//! Orchestrate the three-level verification cascade.
//!
//! Levels:
//! 1. **Model Check** — exhaustive state-space exploration via Stateright
//! 2. **Deterministic Simulation** — FoundationDB/TigerBeetle-style fault injection
//! 3. **Property Tests** — random action sequences with invariant checking
//!
//! Each level produces a pass/fail result. All levels run independently.

use crate::checker::{self, VerificationResult};
use crate::model::{self, TemperModel};
use crate::simulation::{self, SimConfig, SimulationResult};
use crate::proptest_gen::{self, PropTestResult};

use temper_runtime::scheduler::FaultConfig;

/// The levels available in the verification cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeLevel {
    /// Level 1: Exhaustive model checking via Stateright.
    ModelCheck,
    /// Level 2: Deterministic simulation with fault injection.
    Simulation,
    /// Level 3: Property-based testing with random action sequences.
    PropertyTest,
}

impl std::fmt::Display for CascadeLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CascadeLevel::ModelCheck => write!(f, "Level 1: Model Check"),
            CascadeLevel::Simulation => write!(f, "Level 2: Deterministic Simulation"),
            CascadeLevel::PropertyTest => write!(f, "Level 3: Property Tests"),
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
    /// Detailed results (level-specific).
    pub verification: Option<VerificationResult>,
    pub simulation: Option<SimulationResult>,
    pub prop_test: Option<PropTestResult>,
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

/// Orchestrates the three-level verification cascade.
pub struct VerificationCascade {
    tla_source: Option<String>,
    state_machine: Option<temper_spec::tlaplus::StateMachine>,
    max_items: usize,
    /// Number of simulation seeds to test.
    sim_seeds: u64,
    /// Simulation ticks per seed.
    sim_ticks: u64,
    /// Number of property test cases.
    prop_test_cases: u64,
    /// Max steps per property test case.
    prop_test_max_steps: usize,
}

impl VerificationCascade {
    pub fn new(state_machine: temper_spec::tlaplus::StateMachine) -> Self {
        Self {
            tla_source: None,
            state_machine: Some(state_machine),
            max_items: 2,
            sim_seeds: 10,
            sim_ticks: 200,
            prop_test_cases: 1000,
            prop_test_max_steps: 30,
        }
    }

    pub fn from_tla(tla_source: &str) -> Self {
        Self {
            tla_source: Some(tla_source.to_string()),
            state_machine: None,
            max_items: 2,
            sim_seeds: 10,
            sim_ticks: 200,
            prop_test_cases: 1000,
            prop_test_max_steps: 30,
        }
    }

    pub fn with_max_items(mut self, max_items: usize) -> Self {
        self.max_items = max_items;
        self
    }

    pub fn with_sim_seeds(mut self, seeds: u64) -> Self {
        self.sim_seeds = seeds;
        self
    }

    pub fn with_prop_test_cases(mut self, cases: u64) -> Self {
        self.prop_test_cases = cases;
        self
    }

    /// Run the full three-level verification cascade.
    pub fn run(&self) -> CascadeResult {
        let mut levels = Vec::new();
        let tla_source = self.get_tla_source();

        // Level 1: Stateright model checking
        let model = self.build_temper_model();
        let l1 = self.run_model_check(&model);
        levels.push(l1);

        // Level 2: Deterministic simulation
        let l2 = self.run_simulation(&tla_source);
        levels.push(l2);

        // Level 3: Property-based tests
        let l3 = self.run_prop_tests(&tla_source);
        levels.push(l3);

        let all_passed = levels.iter().all(|l| l.passed);
        CascadeResult { all_passed, levels }
    }

    fn get_tla_source(&self) -> String {
        if let Some(ref source) = self.tla_source {
            source.clone()
        } else if let Some(ref sm) = self.state_machine {
            // Reconstruct a minimal TLA+ source for simulation/proptest
            // In practice, the raw source should be passed via from_tla()
            format!("---- MODULE {} ----\n====", sm.module_name)
        } else {
            panic!("VerificationCascade requires TLA+ source or StateMachine");
        }
    }

    fn build_temper_model(&self) -> TemperModel {
        if let Some(ref source) = self.tla_source {
            model::build_model_from_tla(source, self.max_items)
        } else if let Some(ref sm) = self.state_machine {
            model::build_model_with_max_items(sm, self.max_items)
        } else {
            panic!("VerificationCascade requires TLA+ source or StateMachine");
        }
    }

    /// Level 1: Stateright exhaustive model checking.
    fn run_model_check(&self, model: &TemperModel) -> LevelResult {
        let verification = checker::check_model(model);
        let passed = verification.all_properties_hold;
        let summary = if passed {
            format!(
                "L1 Model Check PASSED: {} states explored, all properties hold",
                verification.states_explored,
            )
        } else {
            format!(
                "L1 Model Check FAILED: {} states explored, {} counterexample(s)",
                verification.states_explored,
                verification.counterexamples.len(),
            )
        };

        LevelResult {
            level: CascadeLevel::ModelCheck,
            passed,
            summary,
            verification: Some(verification),
            simulation: None,
            prop_test: None,
        }
    }

    /// Level 2: Deterministic simulation with fault injection.
    fn run_simulation(&self, tla_source: &str) -> LevelResult {
        let base_config = SimConfig {
            seed: 1,
            max_ticks: self.sim_ticks,
            num_actors: 3,
            max_actions_per_actor: 20,
            max_items: self.max_items,
            faults: FaultConfig::light(),
        };

        let results = simulation::run_multi_seed_simulation(tla_source, &base_config, self.sim_seeds);

        let all_passed = results.iter().all(|r| r.all_invariants_held);
        let total_transitions: u64 = results.iter().map(|r| r.total_transitions).sum();
        let total_dropped: u64 = results.iter().map(|r| r.total_dropped).sum();
        let violations: Vec<_> = results.iter()
            .flat_map(|r| r.violations.clone())
            .collect();

        let summary = if all_passed {
            format!(
                "L2 Simulation PASSED: {} seeds, {} transitions, {} dropped msgs",
                self.sim_seeds, total_transitions, total_dropped,
            )
        } else {
            format!(
                "L2 Simulation FAILED: {} violation(s) across {} seeds",
                violations.len(), self.sim_seeds,
            )
        };

        // Return the first result as representative
        let representative = results.into_iter().next();

        LevelResult {
            level: CascadeLevel::Simulation,
            passed: all_passed,
            summary,
            verification: None,
            simulation: representative,
            prop_test: None,
        }
    }

    /// Level 3: Property-based tests.
    fn run_prop_tests(&self, tla_source: &str) -> LevelResult {
        let result = proptest_gen::run_prop_tests_from_tla(tla_source, self.prop_test_cases, self.prop_test_max_steps);
        let passed = result.passed;

        let summary = if passed {
            format!(
                "L3 Property Tests PASSED: {} cases, {} max steps",
                result.total_cases, self.prop_test_max_steps,
            )
        } else {
            let failure_desc = result.failure.as_ref()
                .map(|f| format!("invariant '{}' violated after {} actions", f.invariant, f.action_sequence.len()))
                .unwrap_or_else(|| "unknown failure".to_string());
            format!("L3 Property Tests FAILED: {}", failure_desc)
        };

        LevelResult {
            level: CascadeLevel::PropertyTest,
            passed,
            summary,
            verification: None,
            simulation: None,
            prop_test: Some(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDER_TLA: &str = include_str!("../../../reference/ecommerce/specs/order.tla");

    #[test]
    fn test_full_cascade_passes() {
        let cascade = VerificationCascade::from_tla(ORDER_TLA)
            .with_sim_seeds(5)
            .with_prop_test_cases(100);

        let result = cascade.run();
        assert!(result.all_passed, "Full cascade should pass for reference spec");
        assert_eq!(result.levels.len(), 3);
    }

    #[test]
    fn test_cascade_has_all_three_levels() {
        let cascade = VerificationCascade::from_tla(ORDER_TLA)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);

        let result = cascade.run();

        assert!(result.level_result(CascadeLevel::ModelCheck).is_some());
        assert!(result.level_result(CascadeLevel::Simulation).is_some());
        assert!(result.level_result(CascadeLevel::PropertyTest).is_some());
    }

    #[test]
    fn test_cascade_level_summaries() {
        let cascade = VerificationCascade::from_tla(ORDER_TLA)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);

        let result = cascade.run();

        let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
        assert!(l1.summary.contains("L1"), "Should have L1 prefix");
        assert!(l1.passed);

        let l2 = result.level_result(CascadeLevel::Simulation).unwrap();
        assert!(l2.summary.contains("L2"), "Should have L2 prefix");
        assert!(l2.passed);

        let l3 = result.level_result(CascadeLevel::PropertyTest).unwrap();
        assert!(l3.summary.contains("L3"), "Should have L3 prefix");
        assert!(l3.passed);
    }
}
