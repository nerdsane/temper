//! Structured verification diagnostics emitted before backend execution.

use crate::model::{InvariantKind, TemperModel};
use temper_spec::automaton::SourceSpan;

/// Stable diagnostic code for an invariant outside the verifier's typed IR.
pub const UNSUPPORTED_INVARIANT_CODE: &str = "TVE001";

/// A safety-invariant capability error that blocks the verification cascade.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InvariantCapabilityError {
    /// Stable machine-readable error code.
    pub code: String,
    /// Declared invariant name.
    pub invariant: String,
    /// Unsupported assertion expression.
    pub assertion: String,
    /// Exact half-open source range of `assertion`, excluding TOML quotes.
    pub source_span: SourceSpan,
}

/// Parse an IOA source and collect every unsupported invariant in declaration order.
///
/// This is the cheap, deterministic capability gate used before cached or trusted
/// verification artifacts are accepted. Missing locations are returned as model
/// construction errors rather than panicking or emitting approximate diagnostics.
pub fn unsupported_invariant_errors_from_ioa(
    ioa_source: &str,
) -> Result<Vec<InvariantCapabilityError>, String> {
    let model = crate::model::build_model_from_ioa(ioa_source, 0)?;
    unsupported_invariant_errors(&model)
}

/// Return explicit disclosures for safety assertions enforced only at runtime.
///
/// Cached verification results call this independently so skipping the cascade
/// cannot erase the distinction between runtime enforcement and model proof.
pub fn runtime_enforcement_warnings_from_ioa(ioa_source: &str) -> Result<Vec<String>, String> {
    let model = crate::model::build_model_from_ioa(ioa_source, 0)?;
    Ok(runtime_enforcement_warnings(&model))
}

pub(crate) fn runtime_enforcement_warnings(model: &TemperModel) -> Vec<String> {
    model
        .invariants
        .iter()
        .filter_map(|invariant| {
            let InvariantKind::RuntimeEnforced(_) = &invariant.kind else {
                return None;
            };
            Some(format!(
                "invariant '{}' is enforced by runtime safety contract version {}, not model-proved",
                invariant.name,
                temper_spec::automaton::RUNTIME_INVARIANT_ENFORCEMENT_VERSION
            ))
        })
        .collect()
}

pub(crate) fn unsupported_invariant_errors(
    model: &TemperModel,
) -> Result<Vec<InvariantCapabilityError>, String> {
    model
        .invariants
        .iter()
        .filter_map(|invariant| {
            let InvariantKind::Unverifiable { expression } = &invariant.kind else {
                return None;
            };
            Some(invariant.source_span.map_or_else(
                || {
                    Err(format!(
                        "unsupported IOA invariant '{}' is missing its assertion source span",
                        invariant.name
                    ))
                },
                |source_span| {
                    Ok(InvariantCapabilityError {
                        code: UNSUPPORTED_INVARIANT_CODE.to_string(),
                        invariant: invariant.name.clone(),
                        assertion: expression.clone(),
                        source_span,
                    })
                },
            ))
        })
        .collect::<Result<Vec<_>, _>>()
}

#[cfg(test)]
mod tests {
    use crate::VerificationCascade;

    const UNSUPPORTED_IOA: &str = r#"
[automaton]
name = "Workspace"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "used_bytes"
type = "counter"
initial = "0"

[[state]]
name = "quota_limit"
type = "counter"
initial = "1"

[[invariant]]
name = "WithinQuota"
when = ["Active"]
assert = "used_bytes ** quota_limit"
"#;

    #[test]
    fn every_verification_backend_rejects_unsupported_ir_with_zero_budget() {
        let model = crate::model::build_model_from_ioa(UNSUPPORTED_IOA, 2).expect("build model");

        let symbolic = crate::smt::verify_symbolic(UNSUPPORTED_IOA, 2);
        assert!(!symbolic.all_passed);

        let checked = crate::checker::check_model(&model);
        assert!(!checked.all_properties_hold);
        assert!(
            checked
                .counterexamples
                .iter()
                .any(|counterexample| counterexample.property == "InvariantCapability")
        );

        let simulation = crate::simulation::run_simulation_from_ioa(
            UNSUPPORTED_IOA,
            &crate::simulation::SimConfig {
                max_ticks: 0,
                num_actors: 0,
                max_actions_per_actor: 0,
                ..crate::simulation::SimConfig::default()
            },
        )
        .expect("run simulation");
        assert!(!simulation.all_invariants_held);

        let multi_seed = crate::simulation::run_multi_seed_simulation_from_ioa(
            UNSUPPORTED_IOA,
            &crate::simulation::SimConfig::default(),
            0,
        );
        assert!(multi_seed.is_err());

        let property_tests = crate::proptest_gen::run_prop_tests_from_ioa(UNSUPPORTED_IOA, 0, 0);
        assert!(!property_tests.passed);
    }

    #[test]
    fn capability_preflight_rejects_undeclared_counter_references() {
        for assertion in ["missing > 0", "missing >= 0"] {
            let spec = format!(
                r#"
[automaton]
name = "UndeclaredCounterClaim"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[invariant]]
name = "CounterMustBeDeclared"
when = ["Active"]
assert = "{assertion}"
"#
            );

            let result = VerificationCascade::from_ioa(&spec)
                .with_sim_seeds(0)
                .with_prop_test_cases(0)
                .run();

            assert!(!result.all_passed, "{assertion} must fail closed");
            assert!(result.levels.is_empty());
            assert_eq!(result.errors.len(), 1);
            assert_eq!(result.errors[0].code, "TVE001");
            assert_eq!(result.errors[0].assertion, assertion);
        }
    }

    #[test]
    fn capability_preflight_rejects_undeclared_status_references() {
        for (when, assertion) in [("TypoState", "true"), ("Active", "never(TypoState)")] {
            let spec = format!(
                r#"
[automaton]
name = "UndeclaredStatusClaim"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[invariant]]
name = "StatusMustBeDeclared"
when = ["{when}"]
assert = "{assertion}"
"#
            );

            let result = VerificationCascade::from_ioa(&spec)
                .with_sim_seeds(0)
                .with_prop_test_cases(0)
                .run();

            assert!(
                !result.all_passed,
                "when={when}, assert={assertion} must fail closed"
            );
            assert!(result.levels.is_empty());
            assert_eq!(result.errors.len(), 1);
            assert_eq!(result.errors[0].code, "TVE001");
            assert_eq!(result.errors[0].assertion, assertion);
        }
    }
}
