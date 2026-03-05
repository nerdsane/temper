//! Orchestrate the verification cascade.
//!
//! Levels:
//! 0. **Symbolic Verification** — SMT-based algebraic verification (Z3)
//! 1. **Model Check** — exhaustive state-space exploration via Stateright
//! 2. **Deterministic Simulation** — FoundationDB/TigerBeetle-style fault injection
//!    2b. **Actor Simulation** — real TransitionTable::evaluate() through SimActorSystem
//! 3. **Property Tests** — random action sequences with invariant checking
//!
//! Each level produces a pass/fail result. All levels run independently.

use crate::checker::{self, VerificationResult};
use crate::model::{self, InvariantKind, TemperModel};
use crate::proptest_gen::{self, PropTestResult};
use crate::simulation::{self, SimConfig, SimulationResult};
use crate::smt::{self, SmtResult};

use temper_runtime::scheduler::FaultConfig;

/// Result of an actor simulation level (Level 2b).
///
/// This is provided by the caller since the actor simulation handler lives
/// in `temper-server` (which depends on `temper-verify`, not the other way).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActorSimResult {
    /// Whether all invariants held.
    pub all_invariants_held: bool,
    /// Total transitions across all seeds.
    pub total_transitions: u64,
    /// Total seeds tested.
    pub seeds_tested: u64,
    /// Summary text.
    pub summary: String,
}

/// A function that runs actor simulation and returns the result.
pub type ActorSimRunner = Box<dyn Fn(u64) -> ActorSimResult>;

/// The levels available in the verification cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum CascadeLevel {
    /// Level 0: Symbolic verification via Z3 SMT solver.
    SymbolicVerification,
    /// Level 1: Exhaustive model checking via Stateright.
    ModelCheck,
    /// Level 2: Deterministic simulation with fault injection (model-level).
    Simulation,
    /// Level 2b: Actor simulation — real TransitionTable::evaluate() through SimActorSystem.
    ActorSimulation,
    /// Level 3: Property-based testing with random action sequences.
    PropertyTest,
}

impl std::fmt::Display for CascadeLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CascadeLevel::SymbolicVerification => write!(f, "Level 0: Symbolic Verification"),
            CascadeLevel::ModelCheck => write!(f, "Level 1: Model Check"),
            CascadeLevel::Simulation => write!(f, "Level 2: Deterministic Simulation"),
            CascadeLevel::ActorSimulation => write!(f, "Level 2b: Actor Simulation"),
            CascadeLevel::PropertyTest => write!(f, "Level 3: Property Tests"),
        }
    }
}

/// The result of a single cascade level.
#[derive(Debug, Clone, serde::Serialize)]
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
    pub smt: Option<SmtResult>,
}

/// The aggregate result of running the full verification cascade.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CascadeResult {
    /// Whether all levels passed.
    pub all_passed: bool,
    /// Per-level results.
    pub levels: Vec<LevelResult>,
    /// Warnings about invariants that could not be verified at model level.
    pub warnings: Vec<String>,
    /// Reachable paths extracted after L1 model check (if path extraction was configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachable_paths: Option<crate::paths::PathExtractionResult>,
}

impl CascadeResult {
    /// Return the result for a specific level, if it was run.
    pub fn level_result(&self, level: CascadeLevel) -> Option<&LevelResult> {
        self.levels.iter().find(|r| r.level == level)
    }
}

/// Orchestrates the verification cascade.
pub struct VerificationCascade {
    ioa_source: String,
    max_counter: usize,
    /// Number of simulation seeds to test.
    sim_seeds: u64,
    /// Simulation ticks per seed.
    sim_ticks: u64,
    /// Number of property test cases.
    prop_test_cases: u32,
    /// Max steps per property test case.
    prop_test_max_steps: usize,
    /// Optional actor simulation runner (Level 2b).
    actor_sim_runner: Option<ActorSimRunner>,
    /// If true, stop after the first failing level.
    fail_fast: bool,
    /// Optional path extraction config (runs after L1 passes).
    path_extraction_config: Option<crate::paths::PathExtractionConfig>,
}

impl VerificationCascade {
    /// Create from I/O Automaton TOML source.
    pub fn from_ioa(ioa_toml: &str) -> Self {
        Self {
            ioa_source: ioa_toml.to_string(),
            max_counter: 2,
            sim_seeds: 10,
            sim_ticks: 200,
            prop_test_cases: 1000,
            prop_test_max_steps: 30,
            actor_sim_runner: None,
            fail_fast: false,
            path_extraction_config: None,
        }
    }

    /// Set the actor simulation runner (Level 2b).
    pub fn with_actor_sim(mut self, runner: ActorSimRunner) -> Self {
        self.actor_sim_runner = Some(runner);
        self
    }

    /// Set the maximum counter value for bounded exploration.
    pub fn with_max_items(mut self, max_counter: usize) -> Self {
        self.max_counter = max_counter;
        self
    }

    /// Set the number of simulation seeds.
    pub fn with_sim_seeds(mut self, seeds: u64) -> Self {
        self.sim_seeds = seeds;
        self
    }

    /// Set the number of property test cases.
    pub fn with_prop_test_cases(mut self, cases: u32) -> Self {
        self.prop_test_cases = cases;
        self
    }

    /// Enable fail-fast mode: stop after the first failing level.
    pub fn with_fail_fast(mut self) -> Self {
        self.fail_fast = true;
        self
    }

    /// Enable path extraction after L1 model check passes.
    pub fn with_path_extraction(mut self, config: crate::paths::PathExtractionConfig) -> Self {
        self.path_extraction_config = Some(config);
        self
    }

    /// Run the full verification cascade.
    pub fn run(&self) -> CascadeResult {
        let mut levels = Vec::new();
        let model = self.build_temper_model();

        // Collect warnings for Unverifiable invariants.
        let warnings = collect_unverifiable_warnings(&model);

        // Level 0: SMT symbolic verification
        let l0 = self.run_symbolic_verification();
        let l0_passed = l0.passed;
        levels.push(l0);
        if self.fail_fast && !l0_passed {
            return CascadeResult {
                all_passed: false,
                levels,
                warnings,
                reachable_paths: None,
            };
        }

        // Level 1: Stateright model checking
        let l1 = self.run_model_check(&model);
        let l1_passed = l1.passed;
        levels.push(l1);
        if self.fail_fast && !l1_passed {
            return CascadeResult {
                all_passed: false,
                levels,
                warnings,
                reachable_paths: None,
            };
        }

        // Run path extraction after L1 passes (if configured).
        let reachable_paths = if l1_passed {
            self.path_extraction_config
                .as_ref()
                .map(|config| crate::paths::extract_paths(&model, config))
        } else {
            None
        };

        // Level 2: Deterministic simulation (model-level)
        let l2 = self.run_simulation_level();
        let l2_passed = l2.passed;
        levels.push(l2);
        if self.fail_fast && !l2_passed {
            return CascadeResult {
                all_passed: false,
                levels,
                warnings,
                reachable_paths,
            };
        }

        // Level 2b: Actor simulation (real TransitionTable::evaluate())
        if let Some(ref runner) = self.actor_sim_runner {
            let l2b = self.run_actor_simulation(runner);
            let l2b_passed = l2b.passed;
            levels.push(l2b);
            if self.fail_fast && !l2b_passed {
                return CascadeResult {
                    all_passed: false,
                    levels,
                    warnings,
                    reachable_paths,
                };
            }
        }

        // Level 3: Property-based tests (with shrinking for minimal counterexamples)
        let l3 = self.run_prop_tests_level(&model);
        levels.push(l3);

        let all_passed = levels.iter().all(|l| l.passed);
        CascadeResult {
            all_passed,
            levels,
            warnings,
            reachable_paths,
        }
    }

    fn build_temper_model(&self) -> TemperModel {
        model::build_model_from_ioa(&self.ioa_source, self.max_counter)
    }

    /// Level 0: SMT symbolic verification.
    fn run_symbolic_verification(&self) -> LevelResult {
        let result = smt::verify_symbolic(&self.ioa_source, self.max_counter);
        let passed = result.all_passed;

        let dead_guards: Vec<&str> = result
            .guard_satisfiability
            .iter()
            .filter(|(_, sat)| !sat)
            .map(|(name, _)| name.as_str())
            .collect();
        let non_inductive: Vec<&str> = result
            .inductive_invariants
            .iter()
            .filter(|(_, ind)| !ind)
            .map(|(name, _)| name.as_str())
            .collect();

        let summary = if passed {
            let mut base = format!(
                "L0 Symbolic PASSED: {} guards satisfiable, {} invariants inductive, {} unreachable",
                result.guard_satisfiability.len(),
                result.inductive_invariants.len(),
                result.unreachable_states.len(),
            );
            if result.approximate {
                base.push_str("; approximate model: ");
                base.push_str(&result.approximation_notes.join(" | "));
            }
            base
        } else {
            let mut issues = Vec::new();
            if !dead_guards.is_empty() {
                issues.push(format!("dead guards: {}", dead_guards.join(", ")));
            }
            if !non_inductive.is_empty() {
                issues.push(format!(
                    "non-inductive invariants: {}",
                    non_inductive.join(", ")
                ));
            }
            if result.approximate {
                issues.push(format!(
                    "approximate model: {}",
                    result.approximation_notes.join(" | ")
                ));
            }
            format!("L0 Symbolic WARNINGS: {}", issues.join("; "))
        };

        LevelResult {
            level: CascadeLevel::SymbolicVerification,
            passed,
            summary,
            verification: None,
            simulation: None,
            prop_test: None,
            smt: Some(result),
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
            let mut parts = Vec::new();
            if !verification.counterexamples.is_empty() {
                parts.push(format!(
                    "{} counterexample(s)",
                    verification.counterexamples.len()
                ));
            }
            if !verification.dead_transitions.is_empty() {
                parts.push(format!(
                    "{} dead transition(s): {}",
                    verification.dead_transitions.len(),
                    verification.dead_transitions.join(", ")
                ));
            }
            format!(
                "L1 Model Check FAILED: {} states explored, {}",
                verification.states_explored,
                parts.join("; "),
            )
        };

        LevelResult {
            level: CascadeLevel::ModelCheck,
            passed,
            summary,
            verification: Some(verification),
            simulation: None,
            prop_test: None,
            smt: None,
        }
    }

    /// Level 2: Deterministic simulation with fault injection.
    fn run_simulation_level(&self) -> LevelResult {
        let base_config = SimConfig {
            seed: 1,
            max_ticks: self.sim_ticks,
            num_actors: 3,
            max_actions_per_actor: 20,
            max_counter: self.max_counter,
            faults: FaultConfig::light(),
        };

        let results = simulation::run_multi_seed_simulation_from_ioa(
            &self.ioa_source,
            &base_config,
            self.sim_seeds,
        );

        let invariants_ok = results.iter().all(|r| r.all_invariants_held);
        let liveness_ok = results.iter().all(|r| r.liveness_violations.is_empty());
        let all_passed = invariants_ok && liveness_ok;
        let total_transitions: u64 = results.iter().map(|r| r.total_transitions).sum();
        let total_dropped: u64 = results.iter().map(|r| r.total_dropped).sum();
        let violations: Vec<_> = results.iter().flat_map(|r| r.violations.clone()).collect();
        let liveness_violations: Vec<_> = results
            .iter()
            .flat_map(|r| r.liveness_violations.clone())
            .collect();

        let summary = if all_passed {
            format!(
                "L2 Simulation PASSED: {} seeds, {} transitions, {} dropped msgs",
                self.sim_seeds, total_transitions, total_dropped,
            )
        } else if !invariants_ok {
            format!(
                "L2 Simulation FAILED: {} invariant violation(s) across {} seeds",
                violations.len(),
                self.sim_seeds,
            )
        } else {
            format!(
                "L2 Simulation FAILED: {} liveness violation(s) across {} seeds",
                liveness_violations.len(),
                self.sim_seeds,
            )
        };

        let representative = results.into_iter().next();

        LevelResult {
            level: CascadeLevel::Simulation,
            passed: all_passed,
            summary,
            verification: None,
            simulation: representative,
            prop_test: None,
            smt: None,
        }
    }

    /// Level 3: Property-based tests with shrinking for minimal counterexamples.
    fn run_prop_tests_level(&self, _model: &TemperModel) -> LevelResult {
        let result = proptest_gen::run_prop_tests_with_shrinking_from_ioa(
            &self.ioa_source,
            self.prop_test_cases,
            self.prop_test_max_steps,
        );
        let passed = result.passed;

        let summary = if passed {
            format!(
                "L3 Property Tests PASSED: {} cases, {} max steps",
                result.total_cases, self.prop_test_max_steps,
            )
        } else {
            let failure_desc = result
                .failure
                .as_ref()
                .map(|f| {
                    format!(
                        "invariant '{}' violated after {} actions",
                        f.invariant,
                        f.action_sequence.len()
                    )
                })
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
            smt: None,
        }
    }

    /// Level 2b: Actor simulation with real TransitionTable::evaluate().
    fn run_actor_simulation(&self, runner: &ActorSimRunner) -> LevelResult {
        let result = runner(self.sim_seeds);

        let summary = if result.all_invariants_held {
            format!(
                "L2b Actor Simulation PASSED: {} seeds, {} transitions",
                result.seeds_tested, result.total_transitions,
            )
        } else {
            format!("L2b Actor Simulation FAILED: {}", result.summary)
        };

        LevelResult {
            level: CascadeLevel::ActorSimulation,
            passed: result.all_invariants_held,
            summary,
            verification: None,
            simulation: None,
            prop_test: None,
            smt: None,
        }
    }
}

/// Collect warnings for invariants classified as `Unverifiable`.
fn collect_unverifiable_warnings(model: &TemperModel) -> Vec<String> {
    model
        .invariants
        .iter()
        .filter_map(|inv| {
            if let InvariantKind::Unverifiable { expression } = &inv.kind {
                Some(format!(
                    "invariant '{}' has unverifiable assertion '{}' — skipped at model level",
                    inv.name, expression,
                ))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    #[test]
    fn test_full_cascade_passes_ioa() {
        let cascade = VerificationCascade::from_ioa(ORDER_IOA)
            .with_sim_seeds(5)
            .with_prop_test_cases(100);

        let result = cascade.run();
        for level in &result.levels {
            assert!(level.passed, "IOA cascade level failed: {}", level.summary);
        }
        // L0 + L1 + L2 + L3 = 4 levels
        assert_eq!(result.levels.len(), 4);
    }

    #[test]
    fn test_cascade_has_all_levels() {
        let cascade = VerificationCascade::from_ioa(ORDER_IOA)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);

        let result = cascade.run();

        assert!(
            result
                .level_result(CascadeLevel::SymbolicVerification)
                .is_some()
        );
        assert!(result.level_result(CascadeLevel::ModelCheck).is_some());
        assert!(result.level_result(CascadeLevel::Simulation).is_some());
        assert!(result.level_result(CascadeLevel::PropertyTest).is_some());
    }

    #[test]
    fn test_cascade_level_summaries() {
        let cascade = VerificationCascade::from_ioa(ORDER_IOA)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);

        let result = cascade.run();

        let l0 = result
            .level_result(CascadeLevel::SymbolicVerification)
            .unwrap();
        assert!(l0.summary.contains("L0"), "Should have L0 prefix");
        assert!(l0.passed);

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

    #[test]
    fn test_cascade_warnings_for_unverifiable_invariants() {
        let cascade = VerificationCascade::from_ioa(ORDER_IOA)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);

        let result = cascade.run();
        // Order spec has "payment_captured" which is not a declared bool,
        // so ShipRequiresPayment becomes Unverifiable.
        assert!(
            !result.warnings.is_empty(),
            "Should have warnings for unverifiable invariants"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("ShipRequiresPayment")),
            "Should warn about ShipRequiresPayment, got: {:?}",
            result.warnings,
        );
    }

    #[test]
    fn test_fail_fast_stops_at_first_failure() {
        // Use a spec that will fail L0 (dead guard).
        let broken_spec = r#"
[automaton]
name = "Broken"
states = ["A", "B"]
initial = "A"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[action]]
name = "Go"
from = ["A"]
to = "B"
guard = "count > 9"
"#;
        let cascade = VerificationCascade::from_ioa(broken_spec)
            .with_sim_seeds(1)
            .with_prop_test_cases(10)
            .with_fail_fast();

        let result = cascade.run();
        assert!(!result.all_passed);
        // Should have stopped early — fewer than 4 levels.
        assert!(
            result.levels.len() < 4,
            "fail_fast should stop early, got {} levels",
            result.levels.len(),
        );
    }

    #[test]
    fn test_no_fail_fast_runs_all_levels() {
        let cascade = VerificationCascade::from_ioa(ORDER_IOA)
            .with_sim_seeds(3)
            .with_prop_test_cases(50);

        let result = cascade.run();
        // Without fail_fast, all 4 levels should run.
        assert_eq!(result.levels.len(), 4);
    }
}
