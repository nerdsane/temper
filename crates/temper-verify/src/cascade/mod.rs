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
use crate::model::{self, TemperModel};
use crate::proptest_gen::{self, PropTestResult};
use crate::simulation::{self, SimConfig, SimulationResult};
use crate::smt::{self, SmtResult};

use temper_runtime::scheduler::FaultConfig;

mod diagnostics;

use diagnostics::collect_unsupported_invariant_diagnostics;
pub use diagnostics::{
    SourceSpan, UNSUPPORTED_SAFETY_INVARIANT_CODE, UnsupportedInvariantDiagnostic,
};

/// Result of an actor simulation level (Level 2b).
///
/// This is provided by the caller since the actor simulation handler lives
/// in `temper-server` (which depends on `temper-verify`, not the other way).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CascadeResult {
    /// Whether all levels passed **and** no unsupported safety invariants remain.
    ///
    /// Per ADR-0178, `all_passed` is never true when [`Self::unsupported_invariants`]
    /// is non-empty — capability failure is independent of level exploration.
    pub all_passed: bool,
    /// Per-level results.
    pub levels: Vec<LevelResult>,
    /// Non-fatal advisory messages (e.g. composite plan build issues).
    ///
    /// Unsupported safety assertions are **not** warnings; see
    /// [`Self::unsupported_invariants`].
    pub warnings: Vec<String>,
    /// Safety invariants the verifier cannot encode (ADR-0178 hard failures).
    #[serde(default)]
    pub unsupported_invariants: Vec<UnsupportedInvariantDiagnostic>,
    /// Reachable paths extracted after L1 model check (if path extraction was configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachable_paths: Option<crate::paths::PathExtractionResult>,
    /// Composite trigger-graph report (ADR-0046). Populated when the
    /// cascade was configured with [`VerificationCascade::with_composite_scope`].
    /// Reports what a future joint-state verifier would verify: the set
    /// of participating entities, the trigger edges between them, cycle
    /// detection, and a state-space upper bound for budgeting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite_report: Option<CompositeCascadeReport>,
}

/// Serializable summary of a [`crate::composite::CompositeVerificationPlan`].
///
/// Intended for inclusion in CI output, dashboards, and telemetry. Does
/// not hold the `TemperModel`s themselves — those are re-buildable from
/// the entity type list when a future checker consumes the plan.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompositeCascadeReport {
    /// Seed entity the composite was rooted at.
    pub seed: String,
    /// Entity types in the composition scope (reachable from the seed
    /// via `[[action.triggers]]` kind="entity" edges).
    pub scope: Vec<String>,
    /// Number of trigger edges within the scope.
    pub edge_count: usize,
    /// Whether the trigger graph contains a cycle reachable from the
    /// seed. Legal (cascade depth bounds cycles at runtime) but surfaced
    /// for visibility.
    pub has_cycle: bool,
    /// Conservative upper bound on joint state-space size — product of
    /// per-entity reachable-status counts. Useful for budgeting whether
    /// composite BFS is tractable.
    pub state_space_bound: usize,
    /// Whether any edge in scope requests `liveness = "required"`.
    pub requires_liveness: bool,
    /// Human-readable one-liner for logs / CI output.
    pub summary: String,
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
    /// Optional composite scope (ADR-0046). When set, the cascade
    /// additionally builds a [`crate::composite::CompositeVerificationPlan`]
    /// and reports it alongside single-entity results.
    composite_scope: Option<CompositeScopeConfig>,
}

struct CompositeScopeConfig {
    automatons: Vec<temper_spec::automaton::Automaton>,
    seed: String,
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
            composite_scope: None,
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

    /// Attach additional parsed automatons so the cascade can also build
    /// and report a [`crate::composite::CompositeVerificationPlan`]
    /// rooted at `seed` (ADR-0046). The single-entity cascade still runs
    /// on the `ioa_source` provided at construction time; the composite
    /// plan is an additional report appended to the result as
    /// [`CascadeResult::composite_report`].
    ///
    /// The `seed` entity must exist in `automatons` — otherwise the
    /// composite step records a warning and skips without failing the
    /// overall cascade.
    pub fn with_composite_scope(
        mut self,
        automatons: Vec<temper_spec::automaton::Automaton>,
        seed: impl Into<String>,
    ) -> Self {
        self.composite_scope = Some(CompositeScopeConfig {
            automatons,
            seed: seed.into(),
        });
        self
    }

    /// Run the full verification cascade.
    pub fn run(&self) -> CascadeResult {
        let mut levels = Vec::new();
        let model = self.build_temper_model();

        // ADR-0178 capability gate: unsupported safety is a hard failure,
        // independent of reachability, seeds, or level exploration order.
        let unsupported_invariants =
            collect_unsupported_invariant_diagnostics(&model, &self.ioa_source);
        let mut warnings = Vec::new();

        if self.fail_fast && !unsupported_invariants.is_empty() {
            return CascadeResult {
                all_passed: false,
                levels,
                warnings,
                unsupported_invariants,
                reachable_paths: None,
                composite_report: None,
            };
        }

        // Level 0: SMT symbolic verification
        let l0 = self.run_symbolic_verification();
        let l0_passed = l0.passed;
        levels.push(l0);
        if self.fail_fast && !l0_passed {
            return CascadeResult {
                all_passed: false,
                levels,
                warnings,
                unsupported_invariants,
                reachable_paths: None,
                composite_report: None,
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
                unsupported_invariants,
                reachable_paths: None,
                composite_report: None,
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
                unsupported_invariants,
                reachable_paths,
                composite_report: None,
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
                    composite_report: None,
                    levels,
                    warnings,
                    unsupported_invariants,
                    reachable_paths,
                };
            }
        }

        // Level 3: Property-based tests (with shrinking for minimal counterexamples)
        let l3 = self.run_prop_tests_level(&model);
        levels.push(l3);

        // ADR-0046: build composite report if a scope was configured.
        // Does not fail the cascade — reported as a warning-level
        // enrichment so developers see cross-entity structure alongside
        // single-entity verification.
        let composite_report = self
            .composite_scope
            .as_ref()
            .and_then(|cfg| build_composite_report(cfg, &mut warnings));

        let levels_passed = levels.iter().all(|l| l.passed);
        let all_passed = levels_passed && unsupported_invariants.is_empty();
        CascadeResult {
            all_passed,
            levels,
            warnings,
            unsupported_invariants,
            reachable_paths,
            composite_report,
        }
    }

    fn build_temper_model(&self) -> TemperModel {
        model::build_model_from_ioa(&self.ioa_source, self.max_counter)
            .expect("cascade: IOA spec should have been validated before model building")
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
        )
        .expect("cascade: IOA spec should have been validated before simulation");

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

/// Collect structured diagnostics for invariants classified as `Unverifiable`.
fn build_composite_report(
    cfg: &CompositeScopeConfig,
    warnings: &mut Vec<String>,
) -> Option<CompositeCascadeReport> {
    let automaton_refs: Vec<&temper_spec::automaton::Automaton> = cfg.automatons.iter().collect();
    match crate::composite::CompositeVerificationPlan::new(&automaton_refs, &cfg.seed) {
        Ok(plan) => Some(CompositeCascadeReport {
            seed: plan.seed.clone(),
            scope: plan.models.keys().cloned().collect(),
            edge_count: plan.edge_count(),
            has_cycle: plan.has_cycle,
            state_space_bound: plan.state_space_bound(),
            requires_liveness: plan.requires_liveness(),
            summary: plan.summary(),
        }),
        Err(e) => {
            warnings.push(format!(
                "composite cascade: could not build plan for seed '{}': {e}",
                cfg.seed
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests;
