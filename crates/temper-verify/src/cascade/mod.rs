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
//! [`VerificationCascade::run`] parses IOA at most once (via
//! [`from_automaton`](VerificationCascade::from_automaton) or the
//! [`from_ioa`](VerificationCascade::from_ioa) wrapper) and builds one
//! [`TemperModel`](crate::model::TemperModel) for L0–L3.

mod levels;

use crate::checker::VerificationResult;
use crate::model::{self, InvariantKind, TemperModel};
use crate::proptest_gen::PropTestResult;
use crate::simulation::SimulationResult;
use crate::smt::SmtResult;

use temper_spec::automaton::{parse_automaton, Automaton};

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
    /// Whether all levels passed.
    pub all_passed: bool,
    /// Per-level results.
    pub levels: Vec<LevelResult>,
    /// Warnings about invariants that could not be verified at model level.
    pub warnings: Vec<String>,
    /// Reachable paths extracted after L1 model check (if path extraction was configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachable_paths: Option<crate::paths::PathExtractionResult>,
    /// Composite trigger-graph report (ADR-0046). Populated when the
    /// cascade was configured with [`VerificationCascade::with_composite_scope`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite_report: Option<CompositeCascadeReport>,
}

/// Serializable summary of a [`crate::composite::CompositeVerificationPlan`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompositeCascadeReport {
    /// Seed entity the composite was rooted at.
    pub seed: String,
    /// Entity types in the composition scope (reachable from the seed
    /// via `[[action.triggers]]` kind="entity" edges).
    pub scope: Vec<String>,
    /// Number of trigger edges within the scope.
    pub edge_count: usize,
    /// Whether the trigger graph contains a cycle reachable from the seed.
    pub has_cycle: bool,
    /// Conservative upper bound on joint state-space size.
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
    automaton: Automaton,
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
    /// Optional composite scope (ADR-0046).
    composite_scope: Option<CompositeScopeConfig>,
}

struct CompositeScopeConfig {
    automatons: Vec<Automaton>,
    seed: String,
}

impl VerificationCascade {
    /// Parse I/O Automaton TOML and construct a cascade.
    ///
    /// Returns an error if the TOML fails to parse. Prefer this over
    /// [`from_ioa`](Self::from_ioa) at CLI boundaries (stdin, files).
    pub fn try_from_ioa(ioa_toml: &str) -> Result<Self, String> {
        let automaton = parse_automaton(ioa_toml)
            .map_err(|e| format!("failed to parse I/O Automaton TOML: {e}"))?;
        Ok(Self::from_automaton(automaton))
    }

    /// Parse I/O Automaton TOML and construct a cascade.
    ///
    /// # Panics
    ///
    /// Panics if the TOML fails to parse. Use [`try_from_ioa`](Self::try_from_ioa)
    /// when the source may not be IOA.
    pub fn from_ioa(ioa_toml: &str) -> Self {
        Self::try_from_ioa(ioa_toml)
            .expect("cascade: IOA spec should have been validated before construction")
    }

    /// Construct a cascade from a parsed [`Automaton`].
    ///
    /// [`run`](Self::run) builds one [`TemperModel`] from this automaton and
    /// passes that view to L0–L3. No further TOML parse.
    pub fn from_automaton(automaton: Automaton) -> Self {
        Self {
            automaton,
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
    /// on the automaton provided at construction time; the composite
    /// plan is an additional report appended to the result as
    /// [`CascadeResult::composite_report`].
    ///
    /// The `seed` entity must exist in `automatons` — otherwise the
    /// composite step records a warning and skips without failing the
    /// overall cascade.
    pub fn with_composite_scope(
        mut self,
        automatons: Vec<Automaton>,
        seed: impl Into<String>,
    ) -> Self {
        self.composite_scope = Some(CompositeScopeConfig {
            automatons,
            seed: seed.into(),
        });
        self
    }

    /// Run the full verification cascade against one [`TemperModel`].
    pub fn run(&self) -> CascadeResult {
        let mut levels = Vec::new();
        let model = model::build_model_from_automaton(&self.automaton, self.max_counter);

        let mut warnings = collect_unverifiable_warnings(&model);

        let l0 = self.run_symbolic_verification(&model);
        let l0_passed = l0.passed;
        levels.push(l0);
        if self.fail_fast && !l0_passed {
            return CascadeResult {
                all_passed: false,
                levels,
                warnings,
                reachable_paths: None,
                composite_report: None,
            };
        }

        let l1 = self.run_model_check(&model);
        let l1_passed = l1.passed;
        levels.push(l1);
        if self.fail_fast && !l1_passed {
            return CascadeResult {
                all_passed: false,
                levels,
                warnings,
                reachable_paths: None,
                composite_report: None,
            };
        }

        let reachable_paths = if l1_passed {
            self.path_extraction_config
                .as_ref()
                .map(|config| crate::paths::extract_paths(&model, config))
        } else {
            None
        };

        let l2 = self.run_simulation_level(&model);
        let l2_passed = l2.passed;
        levels.push(l2);
        if self.fail_fast && !l2_passed {
            return CascadeResult {
                all_passed: false,
                levels,
                warnings,
                reachable_paths,
                composite_report: None,
            };
        }

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
                    reachable_paths,
                };
            }
        }

        let l3 = self.run_prop_tests_level(&model);
        levels.push(l3);

        let composite_report = self
            .composite_scope
            .as_ref()
            .and_then(|cfg| build_composite_report(cfg, &mut warnings));

        let all_passed = levels.iter().all(|l| l.passed);
        CascadeResult {
            all_passed,
            levels,
            warnings,
            reachable_paths,
            composite_report,
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

/// Build a [`CompositeCascadeReport`] from the configured scope, appending
/// any build-time warnings (e.g. missing seed) to the cascade's warning
/// list. Returns `None` if the plan cannot be built — the cascade still
/// completes; developers get a non-fatal warning.
fn build_composite_report(
    cfg: &CompositeScopeConfig,
    warnings: &mut Vec<String>,
) -> Option<CompositeCascadeReport> {
    let automaton_refs: Vec<&Automaton> = cfg.automatons.iter().collect();
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
