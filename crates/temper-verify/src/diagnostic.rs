//! Structured verification diagnostics emitted before backend execution.

use crate::model::{InvariantKind, TemperModel};
use temper_spec::automaton::{
    Automaton, SourceSpan, compile_runtime_invariants, parse_automaton,
    unsupported_safety_invariant_names,
};

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
    let automaton = parse_diagnostic_automaton(ioa_source)?;
    let model = crate::model::build_model_from_automaton(&automaton, 0);
    unsupported_invariant_errors_with_automaton(&model, &automaton)
}

/// Return explicit disclosures for safety assertions enforced only at runtime.
///
/// Cached verification results call this independently so skipping the cascade
/// cannot erase the distinction between runtime enforcement and model proof.
pub fn runtime_enforcement_warnings_from_ioa(ioa_source: &str) -> Result<Vec<String>, String> {
    let automaton = parse_diagnostic_automaton(ioa_source)?;
    Ok(runtime_enforcement_warnings_from_automaton(&automaton))
}

fn parse_diagnostic_automaton(ioa_source: &str) -> Result<Automaton, String> {
    parse_automaton(ioa_source)
        .map_err(|error| format!("failed to parse I/O Automaton TOML: {error}"))
}

fn runtime_enforcement_warnings_from_automaton(automaton: &Automaton) -> Vec<String> {
    compile_runtime_invariants(automaton)
        .into_iter()
        .map(|invariant| runtime_enforcement_warning(&invariant.name))
        .collect()
}

fn runtime_enforcement_warning(invariant_name: &str) -> String {
    format!(
        "invariant '{invariant_name}' is enforced by runtime safety contract version {}, not model-proved",
        temper_spec::automaton::RUNTIME_INVARIANT_ENFORCEMENT_VERSION
    )
}

fn unsupported_invariant_errors_with_automaton(
    model: &TemperModel,
    automaton: &Automaton,
) -> Result<Vec<InvariantCapabilityError>, String> {
    let runtime_unsupported = unsupported_safety_invariant_names(automaton);
    let mut errors = Vec::new();
    for declaration in &automaton.invariants {
        let mut matches = model.invariants.iter().filter(|resolved| {
            resolved.name == declaration.name && resolved.source_span == declaration.assert_span
        });
        let resolved = matches.next().ok_or_else(|| {
            format!(
                "verification model did not retain declared safety invariant '{}'",
                declaration.name
            )
        })?;
        if matches.next().is_some() {
            return Err(format!(
                "verification model retained multiple copies of declared safety invariant '{}'",
                declaration.name
            ));
        }
        let model_unsupported = matches!(resolved.kind, InvariantKind::Unverifiable { .. });
        let runtime_unsupported = runtime_unsupported
            .iter()
            .any(|name| name == &declaration.name);
        if model_unsupported || runtime_unsupported {
            let source_span = declaration.assert_span.ok_or_else(|| {
                format!(
                    "unsupported IOA invariant '{}' is missing its assertion source span",
                    declaration.name
                )
            })?;
            errors.push(InvariantCapabilityError {
                code: UNSUPPORTED_INVARIANT_CODE.to_string(),
                invariant: declaration.name.clone(),
                assertion: declaration.assert.clone(),
                source_span,
            });
        }
    }
    Ok(errors)
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
