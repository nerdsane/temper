//! CLI reporting for IOA/CSDL typed-reference contradictions.

use std::collections::BTreeMap;
use std::io::{self, Write};

use temper_spec::automaton::{Automaton, LintSeverity, lint_csdl_reference_contracts};
use temper_spec::csdl::CsdlDocument;

pub(super) fn report_csdl_lints(
    csdl: &CsdlDocument,
    automata: &BTreeMap<String, Automaton>,
    error_lines: &mut Vec<String>,
) -> usize {
    let mut errors = 0;
    for finding in lint_csdl_reference_contracts(csdl, automata) {
        match finding.severity {
            LintSeverity::Error => {
                errors += 1;
                error_lines.push(format!(
                    "{}: {} — {}",
                    finding.entity, finding.code, finding.message
                ));
                writeln!(
                    io::stdout().lock(),
                    "\n  [lint:error] {}: {} — {}",
                    finding.entity,
                    finding.code,
                    finding.message
                )
                .expect("lint output should be writable");
            }
            LintSeverity::Warning => {
                writeln!(
                    io::stdout().lock(),
                    "\n  [lint:warn] {}: {} — {}",
                    finding.entity,
                    finding.code,
                    finding.message
                )
                .expect("lint output should be writable");
            }
        }
    }
    errors
}
