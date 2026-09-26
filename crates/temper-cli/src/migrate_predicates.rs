//! `temper migrate-predicates` subcommand.
//!
//! Converts IOA specs written in the old syntax (guard tables and clauses,
//! `when` + `assert`, `no_further_transitions`, trigger-guard and
//! field-predicate tables, verb and table effects, `[[integration]]` blocks,
//! `trigger` and `emit` effects) to the current grammar. Files are rewritten
//! in place with comments and layout kept; `-` converts stdin to stdout.

use std::io::{Read as _, Write as _};

use anyhow::{Context, Result, bail};
use temper_spec::automaton::legacy::migrate_source;

/// Run the `temper migrate-predicates` subcommand.
///
/// Output goes through locked handles so a closed pipe is an error, not a
/// panic.
pub fn run(paths: &[String], check: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let mut failed = 0usize;
    for path in paths {
        if path == "-" {
            let mut source = String::new();
            std::io::stdin().read_to_string(&mut source)?;
            let migration = migrate_source(&source).map_err(|e| anyhow::anyhow!(e))?;
            for note in &migration.notes {
                writeln!(err, "note: {note}")?;
            }
            out.write_all(migration.source.as_bytes())?;
            continue;
        }
        let source = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        match migrate_source(&source) {
            Ok(migration) => {
                for note in &migration.notes {
                    writeln!(err, "{path}: note: {note}")?;
                }
                if migration.source == source {
                    continue;
                }
                if check {
                    writeln!(out, "{path}: would be converted")?;
                } else {
                    std::fs::write(path, &migration.source)
                        .with_context(|| format!("writing {path}"))?;
                    writeln!(out, "{path}: converted")?;
                }
            }
            Err(error) => {
                failed += 1;
                writeln!(err, "{path}: {error}")?;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} spec(s) could not be converted");
    }
    Ok(())
}
