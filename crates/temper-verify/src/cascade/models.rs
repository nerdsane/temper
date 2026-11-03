//! The models a cascade's levels walk, built from its source.

use super::{CascadeLevel, LevelResult, VerificationCascade};
use crate::budget::Spending;
use crate::model::{self, TemperModel};

impl VerificationCascade {
    /// The model L0-L2 walk and the smaller one L3 walks, each limited by the
    /// budget being spent. The source is anyone's, so one that does not parse
    /// is the first level failed, naming why, never a panic.
    pub(super) fn models(
        &self,
        spending: Option<&Spending>,
    ) -> Result<(TemperModel, TemperModel), Box<LevelResult>> {
        let build = |max_counter: usize| -> Result<TemperModel, String> {
            let model = model::build_model_from_ioa(&self.ioa_source, max_counter)?;
            Ok(match spending {
                Some(spending) => spending.limit(model),
                None => model,
            })
        };
        let both = || -> Result<_, String> { Ok((build(self.max_counter)?, build(2)?)) };
        both().map_err(|error| Box::new(does_not_parse(error)))
    }
}

/// The first level, failed, naming why the spec does not parse. The levels
/// after it have no model to walk, so they do not run.
fn does_not_parse(error: String) -> LevelResult {
    LevelResult {
        level: CascadeLevel::SymbolicVerification,
        passed: false,
        summary: format!("L0 Symbolic FAILED: the spec does not parse: {error}"),
        diagnostics: vec![error],
        verification: None,
        simulation: None,
        prop_test: None,
        smt: None,
    }
}

#[cfg(test)]
mod tests {
    use crate::cascade::VerificationCascade;

    /// A bool written as a string is the old syntax this kernel refuses.
    const OLD_SYNTAX: &str = r#"
[automaton]
name = "Stall"
states = ["Open", "Closed"]
initial = "Open"

[[state]]
name = "stalls"
type = "bool"
initial = "false"

[[action]]
name = "Close"
from = ["Open"]
to = "Closed"
"#;

    #[test]
    fn a_spec_that_does_not_parse_fails_its_first_level_without_panicking() {
        let result = VerificationCascade::from_ioa(OLD_SYNTAX)
            .with_sim_seeds(1)
            .with_prop_test_cases(1)
            .run();
        assert!(!result.all_passed);
        assert_eq!(result.levels.len(), 1, "{result:?}");
        let level = &result.levels[0];
        assert!(!level.passed);
        assert!(
            level.summary.contains("does not parse")
                && level.summary.contains("migrate-predicates"),
            "{}",
            level.summary
        );
        assert!(
            VerificationCascade::from_ioa(OLD_SYNTAX)
                .kept_key()
                .is_none(),
            "a spec that does not parse has no verdict to keep"
        );
    }
}
