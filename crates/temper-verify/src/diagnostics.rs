//! Render cascade failures into lines an author can act on.
//!
//! Every level already computes enough detail to localise a failure: L1 keeps
//! counterexample traces, L2 keeps the violated invariant with the action and
//! the states either side of it, L3 keeps a shrunk action sequence. Until this
//! module existed none of it reached the operator, who saw only a count:
//!
//! ```text
//! [FAIL] L2 Simulation FAILED: 17 invariant violation(s) across 5 seeds
//! ```
//!
//! which names neither the invariant nor the action, and leaves bisecting the
//! spec as the only way forward. That is a bad trade for a human and a
//! dangerous one for an agent: told to make verification pass, and given no
//! idea *which* property broke, the cheapest edit an agent can find is to
//! weaken or delete the invariant. The repository's own rule for this —
//! "fix by root cause, never by weakening the invariant" — is unenforceable
//! advice if the tool will not say which invariant to look at.
//!
//! The renderers here are deliberately dumb: they read the structured results
//! and format them. They never re-run a level and never decide pass/fail.
//!
//! Simulation output is grouped. A broken invariant typically fails on most
//! seeds and most actors, so the raw violation list is long and repetitive; it
//! is grouped by `(invariant, action)` and the distinct groups are reported
//! with a count. What localises a bug is the set of distinct ways it fails,
//! not the multiplicity of any one of them. A model-check path is shown whole:
//! every move on it is one the author has to see to reproduce the failure, and
//! its length is the depth of the state that breaks the rule.

use std::collections::BTreeMap;

use crate::checker::VerificationResult;
use crate::proptest_gen::PropTestResult;
use crate::simulation::SimulationResult;
use crate::smt::SmtResult;

/// Maximum distinct violation groups reported per level.
const MAX_GROUPS: usize = 8;

/// Diagnostics for L0 symbolic verification.
///
/// L0 already names dead guards and the invariants it did not prove in its
/// summary.
/// What the summary cannot carry is *why* a guard is dead, so this adds the
/// one reading that distinguishes the two causes an author confuses: a guard
/// unsatisfiable on its own terms, versus one satisfiable in principle but
/// never reachable because no state it admits is reachable.
pub fn smt_diagnostics(result: &SmtResult) -> Vec<String> {
    let mut lines = Vec::new();

    for (name, _) in result.guard_satisfiability.iter().filter(|(_, sat)| !sat) {
        lines.push(format!(
            "guard '{name}' is unsatisfiable: no reachable state admits it, so the action can never fire"
        ));
    }

    for (name, why) in &result.not_proved {
        lines.push(format!(
            "invariant '{name}' not proved inductive: {why}; the model check decides whether a reachable state breaks it"
        ));
    }

    if !result.unreachable_states.is_empty() {
        lines.push(format!(
            "unreachable states: {}",
            result.unreachable_states.join(", ")
        ));
    }

    if result.approximate && !result.approximation_notes.is_empty() {
        lines.push(format!(
            "model is approximate, so absence of a finding is not proof: {}",
            result.approximation_notes.join(" | ")
        ));
    }

    lines
}

/// Diagnostics for L1 exhaustive model checking.
///
/// Renders each counterexample as the path the model check found, ending at
/// the first state on it that breaks the property, with the invariants false
/// there. The trace is the actionable part: a property name says what broke,
/// the trace says how to reach it.
pub fn model_check_diagnostics(result: &VerificationResult) -> Vec<String> {
    let mut lines = Vec::new();

    for counterexample in result.counterexamples.iter().take(MAX_GROUPS) {
        let broken = if counterexample.violated.is_empty() {
            String::new()
        } else {
            format!(" ({} false)", counterexample.violated.join(", "))
        };
        let steps = counterexample.trace.len().saturating_sub(1);
        if counterexample.property == crate::reaches::REACHES {
            let rule = counterexample.violated.join(", ");
            let targets = crate::lost_places::either(&counterexample.never_reaches);
            let back = counterexample
                .trace
                .split_last()
                .and_then(|((last, _), rest)| rest.iter().rposition(|(state, _)| state == last));
            lines.push(match back {
                Some(back) => format!(
                    "rule '{rule}' broken: this path comes back to step {back} without \
                     reaching {targets}, so a record can go round it forever ({steps} step(s)):"
                ),
                None => format!(
                    "rule '{rule}' broken: this path stops without reaching {targets} \
                     after the rule starts ({steps} step(s)):"
                ),
            });
        } else if counterexample.never_reaches.is_empty() {
            lines.push(format!(
                "property '{}' violated{broken}; path ({steps} step(s)):",
                counterexample.property,
            ));
        } else {
            lines.push(format!(
                "rule '{}' broken: the record at the end of this path can never reach {} \
                 ({steps} step(s)):",
                counterexample.violated.join(", "),
                crate::lost_places::either(&counterexample.never_reaches),
            ));
        }
        lines.extend(render_trace(&counterexample.trace));
    }

    if result.counterexamples.len() > MAX_GROUPS {
        lines.push(format!(
            "... and {} further counterexample(s)",
            result.counterexamples.len() - MAX_GROUPS
        ));
    }

    if !result.dead_transitions.is_empty() {
        lines.push(format!(
            "never enabled on any reachable state: {}",
            result.dead_transitions.join(", ")
        ));
    }

    if !result.is_complete {
        lines.push(
            "exploration stopped before exhausting the state space; absence of a finding is not proof"
                .to_string(),
        );
    }

    lines
}

/// One `state --action` line per step, from the initial state to the one
/// that breaks the property.
fn render_trace(
    trace: &[(
        crate::model::TemperModelState,
        Option<crate::model::TemperModelAction>,
    )],
) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, (state, action)) in trace.iter().enumerate() {
        match action {
            Some(action) => lines.push(format!("    {index}. {state} --{action}")),
            None => lines.push(format!("    {index}. {state}")),
        }
    }
    lines
}

/// One distinct way an invariant broke, with how often it happened.
struct ViolationGroup {
    /// Violated invariant name.
    invariant: String,
    /// The action that broke it.
    action: String,
    /// A representative pre-state, rendered.
    before: String,
    /// A representative post-state, rendered.
    after: String,
    /// The lowest seed that produced this group, for replay.
    seed: u64,
    /// How many violations fell into this group.
    count: usize,
}

/// Diagnostics for L2 deterministic simulation.
///
/// Takes every seed's result, not a representative one. Multi-seed runs fail
/// on an arbitrary subset of seeds, so any single result may be a passing one
/// and report nothing at all.
///
/// Each group carries the seed that produced it, because a seed is the whole
/// reproduction: the simulation is deterministic, so replaying it replays the
/// violation exactly.
pub fn simulation_diagnostics(results: &[SimulationResult]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut groups: BTreeMap<(String, String), ViolationGroup> = BTreeMap::new();

    for result in results {
        for violation in &result.violations {
            let key = (violation.invariant.clone(), violation.action.clone());
            groups
                .entry(key)
                .and_modify(|group| {
                    group.count += 1;
                    if result.seed < group.seed {
                        group.seed = result.seed;
                        group.before = violation.state_before.to_string();
                        group.after = violation.state_after.to_string();
                    }
                })
                .or_insert_with(|| ViolationGroup {
                    invariant: violation.invariant.clone(),
                    action: violation.action.clone(),
                    before: violation.state_before.to_string(),
                    after: violation.state_after.to_string(),
                    seed: result.seed,
                    count: 1,
                });
        }
    }

    let total_groups = groups.len();
    for group in groups.values().take(MAX_GROUPS) {
        lines.push(format!(
            "invariant '{}' broken by {}: {} --> {} (seed {}, {} occurrence(s))",
            group.invariant, group.action, group.before, group.after, group.seed, group.count,
        ));
    }
    if total_groups > MAX_GROUPS {
        lines.push(format!(
            "... and {} further distinct invariant/action combination(s)",
            total_groups - MAX_GROUPS
        ));
    }

    for result in results {
        for violation in &result.liveness_violations {
            lines.push(format!(
                "liveness '{}' violated on {}: {} (final state {}, seed {})",
                violation.property,
                violation.actor_id,
                violation.description,
                violation.final_state,
                result.seed,
            ));
        }
    }

    if !lines.is_empty() {
        lines.push(
            "replay a finding with `SimConfig::with_seed(<seed>)`; simulation is deterministic"
                .to_string(),
        );
    }

    lines
}

/// Diagnostics for L3 property tests.
///
/// The action sequence here has already been shrunk by proptest, so it is the
/// minimal path to the violation and worth printing in full, with every
/// state on it: when L1 stops on its budget, this is the only path.
pub fn prop_test_diagnostics(result: &PropTestResult) -> Vec<String> {
    let Some(failure) = &result.failure else {
        return Vec::new();
    };

    let sequence = if failure.action_sequence.is_empty() {
        "<empty>".to_string()
    } else {
        failure.action_sequence.join(" -> ")
    };

    let mut lines = vec![
        format!(
            "invariant '{}' violated in state {}",
            failure.invariant, failure.final_state
        ),
        format!("shrunk action sequence: {sequence}"),
    ];
    lines.extend(render_trace(&failure.trace));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fields, TemperModelState};
    use crate::simulation::InvariantViolation;

    fn state(status: &str, count: usize) -> TemperModelState {
        let mut counters = Fields::new();
        counters.insert("entry_count", count);
        TemperModelState {
            status: status.into(),
            counters,
            booleans: Fields::new(),
            lists: Fields::new(),
            strings: Fields::new(),
        }
    }

    fn violation(invariant: &str, action: &str) -> InvariantViolation {
        InvariantViolation {
            actor_id: "entity-0".to_string(),
            action: action.to_string(),
            state_before: state("Analyzing", 1),
            state_after: state("Analyzed", 2),
            invariant: invariant.to_string(),
            tick: 7,
        }
    }

    fn sim_result(seed: u64, violations: Vec<InvariantViolation>) -> SimulationResult {
        SimulationResult {
            all_invariants_held: violations.is_empty(),
            ticks: 100,
            total_transitions: 60,
            total_messages: 60,
            total_dropped: 0,
            total_rejected: 0,
            violations,
            liveness_violations: Vec::new(),
            seed,
            actor_final_states: Vec::new(),
        }
    }

    #[test]
    fn names_the_invariant_and_the_action_that_broke_it() {
        let results = vec![sim_result(
            1,
            vec![violation("AnalyzedMeansCurrent", "Succeed")],
        )];
        let lines = simulation_diagnostics(&results);
        let joined = lines.join("\n");
        assert!(joined.contains("AnalyzedMeansCurrent"), "{joined}");
        assert!(joined.contains("Succeed"), "{joined}");
        assert!(joined.contains("Analyzing"), "{joined}");
        assert!(joined.contains("Analyzed"), "{joined}");
    }

    /// The regression this module exists for: a run whose first seed passes and
    /// whose later seed fails must still report the failure. Reporting only a
    /// representative result silently dropped it.
    #[test]
    fn reports_violations_from_a_later_seed_when_the_first_seed_passed() {
        let results = vec![
            sim_result(1, Vec::new()),
            sim_result(2, Vec::new()),
            sim_result(3, vec![violation("AnalyzedMeansCurrent", "Succeed")]),
        ];
        let lines = simulation_diagnostics(&results);
        assert!(!lines.is_empty(), "later-seed violation must be reported");
        assert!(lines.join("\n").contains("seed 3"), "{lines:?}");
    }

    #[test]
    fn groups_repeated_violations_and_keeps_the_lowest_seed() {
        let mut lower_seed = violation("Inv", "Act");
        lower_seed.state_before = state("LowerBefore", 4);
        lower_seed.state_after = state("LowerAfter", 5);
        let results = vec![
            sim_result(4, vec![violation("Inv", "Act")]),
            sim_result(2, vec![lower_seed, violation("Inv", "Act")]),
        ];
        let lines = simulation_diagnostics(&results);
        let first = &lines[0];
        assert!(first.contains("3 occurrence(s)"), "{first}");
        assert!(first.contains("seed 2"), "{first}");
        assert!(
            first.contains("LowerBefore") && first.contains("LowerAfter"),
            "{first}"
        );
        assert!(!first.contains("Analyzing"), "{first}");
    }

    #[test]
    fn long_trace_is_shown_whole() {
        use crate::model::TemperModelAction;
        let mut trace: Vec<_> = (0..20)
            .map(|index| {
                (
                    state("Running", index),
                    Some(TemperModelAction {
                        name: format!("Action{index}").into(),
                        target_state: None,
                        params: Fields::new(),
                    }),
                )
            })
            .collect();
        trace.push((state("Broken", 20), None));
        let lines = render_trace(&trace);
        let output = lines.join("\n");
        assert_eq!(lines.len(), 21, "{output}");
        assert!(
            output.contains("0. Running") && output.contains("--Action0"),
            "{output}"
        );
        assert!(
            output.contains("Action19") && output.contains("20. Broken"),
            "{output}"
        );
        assert!(!output.contains("omitted"), "{output}");
    }

    #[test]
    fn distinct_invariants_are_reported_separately() {
        let results = vec![sim_result(
            1,
            vec![violation("InvA", "Act"), violation("InvB", "Act")],
        )];
        let lines = simulation_diagnostics(&results);
        let joined = lines.join("\n");
        assert!(joined.contains("InvA"), "{joined}");
        assert!(joined.contains("InvB"), "{joined}");
    }

    #[test]
    fn no_violations_renders_nothing() {
        assert!(simulation_diagnostics(&[sim_result(1, Vec::new())]).is_empty());
    }

    #[test]
    fn prop_test_renders_the_shrunk_sequence() {
        let result = PropTestResult {
            total_cases: 100,
            passed: false,
            failure: Some(crate::proptest_gen::PropTestFailure {
                invariant: "AnalyzedMeansCurrent".to_string(),
                action_sequence: vec!["Observe".to_string(), "Succeed".to_string()],
                final_state: "Analyzed(entry_count=2)".to_string(),
                trace: Vec::new(),
            }),
        };
        let lines = prop_test_diagnostics(&result);
        let joined = lines.join("\n");
        assert!(joined.contains("Observe -> Succeed"), "{joined}");
        assert!(joined.contains("AnalyzedMeansCurrent"), "{joined}");
    }

    #[test]
    fn prop_test_pass_renders_nothing() {
        let result = PropTestResult {
            total_cases: 100,
            passed: true,
            failure: None,
        };
        assert!(prop_test_diagnostics(&result).is_empty());
    }
}
