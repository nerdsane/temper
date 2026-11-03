//! A filename-derived type name must not make a cross-type trigger local.

use super::*;
use std::collections::BTreeMap;

const SOURCE: &str = r#"[automaton]
name = "Ticket"
states = ["Open"]
initial = "Open"
strict_action_params = true
allow_indefinite_states = ["Open"]
[[action]]
name = "Diagnosed"
kind = "input"
from = ["Open"]
to = "Open"
[[action.triggers]]
name = "retry"
kind = "entity"
target_entity = "Ticket"
target_action = "Retry"
[action.triggers.resolve_target]
kind = "same_id"
[action.triggers.args]
note = "'fixed'"
[[action]]
name = "Retry"
kind = "input"
from = ["Open"]
to = "Open"
"#;

#[test]
fn trigger_args_lint_uses_the_loaders_registered_type() {
    let csdl = temper_spec::csdl::parse_csdl(include_str!(
        "../../../../test-fixtures/specs/model.csdl.xml"
    ))
    .unwrap();
    for (registered_type, errors) in [("ArchivedTicket", 0), ("Ticket", 1)] {
        let findings = lint_loaded_specs(
            &csdl,
            &BTreeMap::from([(registered_type.to_string(), SOURCE.to_string())]),
        )
        .unwrap();
        assert_eq!(
            findings
                .iter()
                .filter(|f| f.code == "trigger_target_arg_undeclared"
                    && f.severity == LintSeverity::Error)
                .count(),
            errors
        );
    }
}
