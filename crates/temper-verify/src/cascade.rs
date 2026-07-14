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

/// Stable error code for unsupported safety-invariant diagnostics (ADR-0178).
pub const UNSUPPORTED_SAFETY_INVARIANT_CODE: &str = "VERIFY_UNSUPPORTED_SAFETY_INVARIANT";

/// Byte and 1-based line/column span into the submitted IOA document.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceSpan {
    /// Inclusive start offset in UTF-8 bytes.
    pub start_byte: usize,
    /// Exclusive end offset in UTF-8 bytes.
    pub end_byte: usize,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based start column (UTF-8 bytes within the line).
    pub start_column: u32,
    /// 1-based end line.
    pub end_line: u32,
    /// 1-based end column (UTF-8 bytes within the line; exclusive).
    pub end_column: u32,
}

/// Structured diagnostic for a safety invariant the verifier cannot encode.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnsupportedInvariantDiagnostic {
    /// Stable machine-readable code ([`UNSUPPORTED_SAFETY_INVARIANT_CODE`]).
    pub code: String,
    /// `[[invariant]]` name from the submitted document.
    pub invariant_name: String,
    /// Original assertion expression that could not be verified.
    pub expression: String,
    /// Source range of the `[[invariant]]` table in the submitted IOA, when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_span: Option<SourceSpan>,
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
fn collect_unsupported_invariant_diagnostics(
    model: &TemperModel,
    ioa_source: &str,
) -> Vec<UnsupportedInvariantDiagnostic> {
    model
        .invariants
        .iter()
        .filter_map(|inv| {
            if let InvariantKind::Unverifiable { expression } = &inv.kind {
                Some(UnsupportedInvariantDiagnostic {
                    code: UNSUPPORTED_SAFETY_INVARIANT_CODE.to_string(),
                    invariant_name: inv.name.clone(),
                    expression: expression.clone(),
                    source_span: find_invariant_source_span(ioa_source, &inv.name),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Locate the `[[invariant]]` array-table for `name` in the submitted IOA TOML.
///
/// Returns the span covering the table header through the last non-empty line
/// before the next top-level table header (`[` …). Matching is by the first
/// `name = "…"` (or `name = '…'`) assignment inside each invariant table.
fn find_invariant_source_span(source: &str, name: &str) -> Option<SourceSpan> {
    let bytes = source.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = source[search_from..].find("[[invariant]]") {
        let table_start = search_from + rel;
        let after_header = table_start + "[[invariant]]".len();
        let next_table = source[after_header..]
            .find("\n[")
            .map(|i| after_header + i)
            .unwrap_or(source.len());
        let table_body = &source[table_start..next_table];
        if invariant_table_name_matches(table_body, name) {
            let end = trim_trailing_ws_end(bytes, next_table);
            return Some(byte_range_to_source_span(source, table_start, end));
        }
        search_from = after_header;
    }
    None
}

fn invariant_table_name_matches(table_body: &str, name: &str) -> bool {
    for line in table_body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                let value = rest
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| rest.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
                if value == Some(name) {
                    return true;
                }
            }
        }
    }
    false
}

fn trim_trailing_ws_end(bytes: &[u8], end: usize) -> usize {
    let mut e = end;
    while e > 0 && matches!(bytes[e - 1], b' ' | b'\t' | b'\n' | b'\r') {
        e -= 1;
    }
    e
}

fn byte_range_to_source_span(source: &str, start_byte: usize, end_byte: usize) -> SourceSpan {
    let (start_line, start_column) = byte_offset_to_line_col(source, start_byte);
    let (end_line, end_column) = byte_offset_to_line_col(source, end_byte);
    SourceSpan {
        start_byte,
        end_byte,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

/// Convert a UTF-8 byte offset into 1-based line and column (byte column).
fn byte_offset_to_line_col(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let mut line = 1u32;
    let mut col = 1u32;
    for (i, b) in source.bytes().enumerate() {
        if i == offset {
            return (line, col);
        }
        if b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
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
    fn test_cascade_fails_closed_on_unsupported_safety_invariant() {
        // Counter-to-counter comparison is not in the verifier capability set
        // (counter-to-literal only). Must fail closed independent of seeds.
        let unsupported = r#"
[automaton]
name = "Workspace"
states = ["Active", "Archived"]
initial = "Active"

[[state]]
name = "used_bytes"
type = "counter"
initial = "0"

[[state]]
name = "quota_limit"
type = "counter"
initial = "0"

[[action]]
name = "Archive"
from = ["Active"]
to = "Archived"

[[invariant]]
name = "UsageBelowQuota"
when = ["Active"]
assert = "used_bytes <= quota_limit"
"#;
        let result = VerificationCascade::from_ioa(unsupported)
            .with_sim_seeds(3)
            .with_prop_test_cases(20)
            .run();

        assert!(
            !result.all_passed,
            "unsupported safety must not report cascade success"
        );
        assert_eq!(result.unsupported_invariants.len(), 1);
        let diag = &result.unsupported_invariants[0];
        assert_eq!(diag.code, UNSUPPORTED_SAFETY_INVARIANT_CODE);
        assert_eq!(diag.invariant_name, "UsageBelowQuota");
        assert_eq!(diag.expression, "used_bytes <= quota_limit");
        let span = diag
            .source_span
            .as_ref()
            .expect("source span for named invariant");
        assert!(
            span.start_byte < span.end_byte,
            "span should cover the invariant table"
        );
        assert!(span.start_line >= 1);
        let slice = &unsupported[span.start_byte..span.end_byte];
        assert!(
            slice.contains("UsageBelowQuota") && slice.contains("used_bytes <= quota_limit"),
            "span should cover name and assert, got: {slice:?}"
        );
        // Must not be described as a soft skip warning.
        assert!(
            result
                .warnings
                .iter()
                .all(|w| !w.contains("skipped at model level")),
            "unsupported safety must not be warning-only: {:?}",
            result.warnings
        );
    }

    #[test]
    fn test_cascade_unsupported_span_multiline_and_repeated() {
        let src = r#"
[automaton]
name = "Multi"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]
to = "B"

[[invariant]]
name = "FirstBad"
assert = "alpha <= beta"

[[invariant]]
name = "OkNever"
assert = "never(B)"

[[invariant]]
name = "SecondBad"
assert = "gamma + delta"
"#;
        let result = VerificationCascade::from_ioa(src)
            .with_sim_seeds(1)
            .with_prop_test_cases(5)
            .run();
        assert!(!result.all_passed);
        assert_eq!(result.unsupported_invariants.len(), 2);
        assert_eq!(result.unsupported_invariants[0].invariant_name, "FirstBad");
        assert_eq!(result.unsupported_invariants[1].invariant_name, "SecondBad");
        for diag in &result.unsupported_invariants {
            let span = diag.source_span.as_ref().expect("span");
            let slice = &src[span.start_byte..span.end_byte];
            assert!(
                slice.contains(&diag.invariant_name),
                "span for {} must include its name: {slice:?}",
                diag.invariant_name
            );
        }
        // Distinct spans for the two unsupported tables.
        let a = result.unsupported_invariants[0]
            .source_span
            .as_ref()
            .unwrap();
        let b = result.unsupported_invariants[1]
            .source_span
            .as_ref()
            .unwrap();
        assert!(a.end_byte <= b.start_byte || b.end_byte <= a.start_byte);
    }

    #[test]
    fn test_cascade_fully_supported_spec_passes() {
        let result = VerificationCascade::from_ioa(ORDER_IOA)
            .with_sim_seeds(3)
            .with_prop_test_cases(50)
            .run();
        assert!(
            result.unsupported_invariants.is_empty(),
            "ORDER fixture must be fully supported after payment_captured was declared: {:?}",
            result.unsupported_invariants
        );
        assert!(
            result.all_passed,
            "supported ORDER cascade should pass, levels: {:?}",
            result
                .levels
                .iter()
                .map(|l| (&l.summary, l.passed))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_fail_fast_stops_on_unsupported_before_levels() {
        let unsupported = r#"
[automaton]
name = "Bad"
states = ["A"]
initial = "A"

[[invariant]]
name = "Mystery"
assert = "not_a_real_expression(x)"
"#;
        let result = VerificationCascade::from_ioa(unsupported)
            .with_fail_fast()
            .run();
        assert!(!result.all_passed);
        assert!(
            result.levels.is_empty(),
            "fail_fast capability gate should skip level exploration"
        );
        assert_eq!(result.unsupported_invariants.len(), 1);
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

    // ─── ADR-0046: composite cascade integration tests ─────────────────

    #[test]
    fn cascade_reports_composite_when_scope_configured() {
        use temper_spec::automaton::parse_automaton;

        let order_spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "confirm_triggers_auth"
kind = "entity"
principal = "payment-service"
target_entity = "Payment"
target_action = "AuthorizePayment"

[action.triggers.resolve_target]
type = "same_id"
"#;
        let payment_spec = r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
"#;
        let order = parse_automaton(order_spec).unwrap();
        let payment = parse_automaton(payment_spec).unwrap();

        let cascade = VerificationCascade::from_ioa(order_spec)
            .with_sim_seeds(2)
            .with_prop_test_cases(10)
            .with_composite_scope(vec![order, payment], "Order");

        let result = cascade.run();
        let report = result
            .composite_report
            .expect("composite scope was configured");
        assert_eq!(report.seed, "Order");
        assert!(report.scope.contains(&"Order".to_string()));
        assert!(report.scope.contains(&"Payment".to_string()));
        assert_eq!(report.edge_count, 1);
        assert!(!report.has_cycle);
        assert!(report.summary.contains("Order"));
    }

    #[test]
    fn cascade_without_composite_scope_has_none_report() {
        let cascade = VerificationCascade::from_ioa(ORDER_IOA)
            .with_sim_seeds(2)
            .with_prop_test_cases(10);
        let result = cascade.run();
        assert!(result.composite_report.is_none());
    }

    #[test]
    fn cascade_composite_missing_seed_records_warning_not_failure() {
        use temper_spec::automaton::parse_automaton;
        let order_spec = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "A"
from = ["Draft"]
"#;
        let order = parse_automaton(order_spec).unwrap();

        let cascade = VerificationCascade::from_ioa(order_spec)
            .with_sim_seeds(2)
            .with_prop_test_cases(10)
            .with_composite_scope(vec![order], "NotAnEntity");

        let result = cascade.run();
        assert!(result.composite_report.is_none());
        assert!(
            result.warnings.iter().any(|w| w.contains("NotAnEntity")),
            "warning should mention missing seed. Got: {:?}",
            result.warnings
        );
    }
}
