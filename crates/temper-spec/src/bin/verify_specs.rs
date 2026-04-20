//! CI gate for entity specifications (ADR-0050).
//!
//! Walks one or more directories, parses every `.ioa.toml` file encountered,
//! and enforces:
//!
//! 1. Basic syntactic/semantic validation (undeclared states, missing
//!    action references, bad guards, etc.) — already in `parse_automaton`.
//! 2. Liveness coverage (ADR-0050) — every non-terminal state must have a
//!    `[[state_timeout]]` or an `allow_indefinite_states` entry.
//!
//! Exit codes:
//! - `0` — every spec passed both gates.
//! - `1` — at least one spec failed; details printed to stderr.
//! - `2` — invocation error (no dirs supplied, unreadable path).
//!
//! Intended use in CI:
//!
//! ```sh
//! cargo run --quiet --bin verify_specs -- os-apps crates
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use temper_spec::automaton::{LivenessEnforcement, parse_automaton_with_liveness};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "usage: verify_specs <dir> [<dir> ...]\n\
             Walks each directory recursively, parses every *.ioa.toml, and\n\
             enforces ADR-0050 liveness coverage. Exits non-zero on any failure."
        );
        return ExitCode::from(2);
    }

    let mut specs_found: u64 = 0;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();

    for raw in &args {
        let root = PathBuf::from(raw);
        if !root.exists() {
            eprintln!("verify_specs: path does not exist: {}", root.display());
            return ExitCode::from(2);
        }
        if let Err(e) = walk(&root, &mut specs_found, &mut failures) {
            eprintln!("verify_specs: walk failed under {}: {e}", root.display());
            return ExitCode::from(2);
        }
    }

    if failures.is_empty() {
        println!(
            "verify_specs: {} spec(s) passed (ADR-0050 liveness enforce mode)",
            specs_found
        );
        return ExitCode::SUCCESS;
    }

    eprintln!(
        "\nverify_specs: {} spec(s) checked, {} failure(s):\n",
        specs_found,
        failures.len()
    );
    for (path, err) in &failures {
        eprintln!("── {}", path.display());
        for line in err.lines() {
            eprintln!("   {line}");
        }
        eprintln!();
    }
    ExitCode::from(1)
}

fn walk(
    dir: &Path,
    specs_found: &mut u64,
    failures: &mut Vec<(PathBuf, String)>,
) -> std::io::Result<()> {
    if dir.is_file() {
        if is_ioa_spec(dir) {
            check_spec(dir, specs_found, failures);
        }
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Skip build artefacts + vendored worktrees to keep CI fast.
            if let Some(name) = path.file_name().and_then(|s| s.to_str())
                && matches!(name, "target" | ".git" | "node_modules" | ".worktrees")
            {
                continue;
            }
            walk(&path, specs_found, failures)?;
        } else if is_ioa_spec(&path) {
            check_spec(&path, specs_found, failures);
        }
    }
    Ok(())
}

fn is_ioa_spec(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.ends_with(".ioa.toml"))
}

fn check_spec(path: &Path, specs_found: &mut u64, failures: &mut Vec<(PathBuf, String)>) {
    *specs_found += 1;
    let Ok(source) = std::fs::read_to_string(path) else {
        failures.push((path.to_path_buf(), "could not read file".to_string()));
        return;
    };
    if let Err(e) = parse_automaton_with_liveness(&source, LivenessEnforcement::Enforce) {
        failures.push((path.to_path_buf(), e.to_string()));
    }
}
