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

use crate::budget::{Budget, Clock, Spending, ran_out, too_many_choices};
use crate::checker::{self, VerificationResult};
use crate::diagnostics;
use crate::lost_places::LostPlace;
use crate::model::{self, TemperModel};
use crate::proptest_gen::{self, PropTestResult};
use crate::simulation::{self, SimConfig, SimulationResult};
use crate::smt::{self, SmtResult};

use temper_runtime::scheduler::FaultConfig;

mod digest;
mod models;
pub use digest::KeptKey;

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
    /// Actionable detail lines for a failure: the violated invariant, the
    /// action that broke it, the states either side, the seed to replay.
    ///
    /// Empty on a passing level. Rendered by [`crate::diagnostics`] from the
    /// level-specific results below, which already carry the detail; before
    /// this field existed that detail was computed and then discarded, leaving
    /// a bare count as the only output. `#[serde(default)]` so a result
    /// decoded from an older server still deserialises.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    /// Detailed results (level-specific).
    pub verification: Option<VerificationResult>,
    pub simulation: Option<SimulationResult>,
    pub prop_test: Option<PropTestResult>,
    pub smt: Option<SmtResult>,
}

/// The aggregate result of running the full verification cascade.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CascadeResult {
    /// Whether all levels passed.
    pub all_passed: bool,
    /// Per-level results.
    pub levels: Vec<LevelResult>,
    /// Warnings raised while verifying (for example, composite-plan issues).
    pub warnings: Vec<String>,
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
    /// Set when the spec was too large to check: the cascade ran out of its
    /// [`Budget`], or a move has more parameter choices than one step explores.
    /// The type's name and the summary of the level that stopped; the levels
    /// after it did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    /// Set when this verdict was not run again but kept: an earlier check
    /// passed a spec whose levels read the same models with the same
    /// settings. The digest of what they read ([`VerificationCascade::digest`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
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
    /// seed. Legal (at runtime a chain round a cycle stops at its first
    /// repeated step) but surfaced for visibility.
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
    /// Optional budget: what the cascade may spend before it stops.
    budget: Option<Budget>,
    /// Where the budget reads the time.
    clock: Clock,
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
            budget: None,
            clock: Clock::Wall,
        }
    }

    /// Stop at `budget`: the level running when it runs out fails, saying so,
    /// and the levels after it do not run.
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Read the budget's time from `clock` instead of the wall clock.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
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
        let spending = self
            .budget
            .map(|budget| Spending::start(budget, self.clock));
        let spending = spending.as_ref();
        let mut levels = Vec::new();
        let (model, prop_model) = match self.models(spending) {
            Ok(models) => models,
            Err(failed) => return finish(vec![*failed], Vec::new(), None, None),
        };

        let mut warnings = Vec::new();

        // A move with more parameter choices than one step explores would stop
        // every level that steps it at an assertion: say so instead.
        if let Some((action, choices)) = model::semantics::oversized_transition(&model)
            .or_else(|| model::semantics::oversized_transition(&prop_model))
        {
            levels.push(too_many_choices(action, choices));
            return self.stopped(levels, warnings);
        }

        // Level 0: SMT symbolic verification
        let l0 = self.run_symbolic_verification();
        let l0_passed = l0.passed;
        levels.push(l0);
        if ran_out(spending, &mut levels, None) {
            return self.stopped(levels, warnings);
        }
        if self.fail_fast && !l0_passed {
            return finish(levels, warnings, None, None);
        }

        // Level 1: Stateright model checking
        let l1 = self.run_model_check(&model);
        let l1_passed = l1.passed;
        let explored = l1
            .verification
            .as_ref()
            .map(|v| (v.states_explored, v.is_complete));
        levels.push(l1);
        if ran_out(spending, &mut levels, explored) {
            return self.stopped(levels, warnings);
        }
        if self.fail_fast && !l1_passed {
            return finish(levels, warnings, None, None);
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
        let l2 = self.run_simulation_level(&model);
        let l2_passed = l2.passed;
        levels.push(l2);
        if ran_out(spending, &mut levels, None) {
            return self.stopped(levels, warnings);
        }
        if self.fail_fast && !l2_passed {
            return finish(levels, warnings, reachable_paths, None);
        }

        // Level 2b: Actor simulation (real TransitionTable::evaluate())
        if let Some(ref runner) = self.actor_sim_runner {
            let l2b = self.run_actor_simulation(runner);
            let l2b_passed = l2b.passed;
            levels.push(l2b);
            if ran_out(spending, &mut levels, None) {
                return self.stopped(levels, warnings);
            }
            if self.fail_fast && !l2b_passed {
                return finish(levels, warnings, reachable_paths, None);
            }
        }

        // Level 3: Property-based tests (with shrinking for minimal counterexamples)
        let l3 = self.run_prop_tests_level(&prop_model);
        levels.push(l3);
        if ran_out(spending, &mut levels, None) {
            return self.stopped(levels, warnings);
        }

        // ADR-0046: build composite report if a scope was configured.
        // Does not fail the cascade — reported as a warning-level
        // enrichment so developers see cross-entity structure alongside
        // single-entity verification.
        let composite_report = self
            .composite_scope
            .as_ref()
            .and_then(|cfg| build_composite_report(cfg, &mut warnings));

        finish(levels, warnings, reachable_paths, composite_report)
    }

    /// The result of a cascade whose budget ran out in its last level.
    fn stopped(&self, levels: Vec<LevelResult>, warnings: Vec<String>) -> CascadeResult {
        let name = temper_spec::automaton::parse_automaton(&self.ioa_source)
            .map(|automaton| automaton.automaton.name)
            .unwrap_or_default();
        let summary = levels.last().map_or("", |level| level.summary.as_str());
        let stopped = Some(format!("{name}: {summary}"));
        CascadeResult {
            stopped,
            ..finish(levels, warnings, None, None)
        }
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
        let proved = result
            .inductive_invariants
            .iter()
            .filter(|(_, ind)| *ind)
            .count();
        let not_proved: Vec<String> = result
            .inductive_invariants
            .iter()
            .filter(|(_, ind)| !ind)
            .map(
                |(name, _)| match result.not_proved.iter().find(|(n, _)| n == name) {
                    Some((_, why)) => format!("{name} ({why})"),
                    None => name.clone(),
                },
            )
            .collect();

        let mut summary = if passed {
            format!(
                "L0 Symbolic PASSED: {} guards satisfiable, {proved} of {} invariants proved inductive, {} unreachable",
                result.guard_satisfiability.len(),
                result.inductive_invariants.len(),
                result.unreachable_states.len(),
            )
        } else {
            format!(
                "L0 Symbolic FAILED: dead guards: {}",
                dead_guards.join(", ")
            )
        };
        if !not_proved.is_empty() {
            summary.push_str("; not proved, left to the model check: ");
            summary.push_str(&not_proved.join(", "));
        }
        if result.approximate {
            summary.push_str("; approximate model: ");
            summary.push_str(&result.approximation_notes.join(" | "));
        }

        LevelResult {
            level: CascadeLevel::SymbolicVerification,
            passed,
            summary,
            diagnostics: if passed {
                Vec::new()
            } else {
                diagnostics::smt_diagnostics(&result)
            },
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
        let summary = if passed && verification.bounded_by.is_empty() {
            format!(
                "L1 Model Check PASSED: {} states explored, all properties hold",
                verification.states_explored,
            )
        } else if passed {
            let bounds: Vec<String> = verification
                .bounded_by
                .iter()
                .map(|(name, bound)| format!("{name} up to {bound}"))
                .collect();
            format!(
                "L1 Model Check PASSED, bounded: {} states explored; every property holds with {}, \
                 and a move can go past that, which was not explored, so this is not a proof",
                verification.states_explored,
                bounds.join(", "),
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
            parts.extend(verification.unproved_can_reach.iter().cloned());
            format!(
                "L1 Model Check FAILED: {} states explored, {}",
                verification.states_explored,
                parts.join("; "),
            )
        };
        // A failed check's failures are the whole answer: a note would often
        // name the record a failure already names.
        let summary = if !passed || verification.lost_places.is_empty() {
            summary
        } else {
            let found: Vec<String> = verification
                .lost_places
                .iter()
                .map(LostPlace::describe)
                .collect();
            format!(
                "{summary}. Not a failure, but worth a look: {}. If that is intended, nothing to do.",
                found.join("; "),
            )
        };

        LevelResult {
            level: CascadeLevel::ModelCheck,
            passed,
            summary,
            diagnostics: if passed {
                Vec::new()
            } else {
                diagnostics::model_check_diagnostics(&verification)
            },
            verification: Some(verification),
            simulation: None,
            prop_test: None,
            smt: None,
        }
    }

    /// Level 2: Deterministic simulation with fault injection.
    fn run_simulation_level(&self, model: &TemperModel) -> LevelResult {
        let base_config = SimConfig {
            seed: 1,
            max_ticks: self.sim_ticks,
            num_actors: 3,
            max_actions_per_actor: 20,
            max_counter: self.max_counter,
            faults: FaultConfig::light(),
        };

        let results = simulation::run_multi_seed_simulation(model, &base_config, self.sim_seeds);

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

        // Name the broken invariants in the summary itself. A count answers
        // "is it broken"; only a name answers "what do I look at", and the
        // summary is the one line every caller prints.
        let mut broken: Vec<&str> = violations.iter().map(|v| v.invariant.as_str()).collect();
        broken.sort_unstable();
        broken.dedup();

        let summary = if all_passed {
            format!(
                "L2 Simulation PASSED: {} seeds, {} transitions, {} dropped msgs",
                self.sim_seeds, total_transitions, total_dropped,
            )
        } else if !invariants_ok {
            format!(
                "L2 Simulation FAILED: {} invariant violation(s) across {} seeds; broken: {}",
                violations.len(),
                self.sim_seeds,
                broken.join(", "),
            )
        } else {
            let mut properties: Vec<&str> = liveness_violations
                .iter()
                .map(|v| v.property.as_str())
                .collect();
            properties.sort_unstable();
            properties.dedup();
            format!(
                "L2 Simulation FAILED: {} liveness violation(s) across {} seeds; broken: {}",
                liveness_violations.len(),
                self.sim_seeds,
                properties.join(", "),
            )
        };

        let diagnostics = if all_passed {
            Vec::new()
        } else {
            diagnostics::simulation_diagnostics(&results)
        };

        let representative = failing_representative(results);

        LevelResult {
            level: CascadeLevel::Simulation,
            passed: all_passed,
            summary,
            diagnostics,
            verification: None,
            simulation: representative,
            prop_test: None,
            smt: None,
        }
    }

    /// Level 3: Property-based tests with shrinking for minimal counterexamples.
    fn run_prop_tests_level(&self, model: &TemperModel) -> LevelResult {
        let result = proptest_gen::run_prop_tests_with_shrinking_on_model(
            model,
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
            diagnostics: if passed {
                Vec::new()
            } else {
                diagnostics::prop_test_diagnostics(&result)
            },
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
            diagnostics: Vec::new(),
            verification: None,
            simulation: None,
            prop_test: None,
            smt: None,
        }
    }
}

/// The result worth attaching to a level: a failing seed's, when one failed.
///
/// Seeds fail independently, so `results[0]` is frequently a passing run whose
/// `violations` list is empty. Attaching it left a failed level carrying no
/// violations at all -- which reads as a tooling bug, and sent the reader back
/// to bisecting the spec by hand. Falls back to the first result so a passing
/// run still reports its transition counts.
fn failing_representative(results: Vec<SimulationResult>) -> Option<SimulationResult> {
    results
        .iter()
        .find(|r| !r.all_invariants_held || !r.liveness_violations.is_empty())
        .cloned()
        .or_else(|| results.into_iter().next())
}

fn finish(
    levels: Vec<LevelResult>,
    warnings: Vec<String>,
    reachable_paths: Option<crate::paths::PathExtractionResult>,
    composite_report: Option<CompositeCascadeReport>,
) -> CascadeResult {
    CascadeResult {
        all_passed: levels.iter().all(|l| l.passed),
        levels,
        warnings,
        reachable_paths,
        composite_report,
        stopped: None,
        kept: None,
    }
}

/// Build a [`CompositeCascadeReport`] from the configured scope, appending
/// any build-time warnings (e.g. missing seed) to the cascade's warning
/// list. Returns `None` if the plan cannot be built — the cascade still
/// completes; developers get a non-fatal warning.
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
#[path = "cascade/cascade_test.rs"]
mod tests;
