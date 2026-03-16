//! `temper verify-ioa` subcommand.
//!
//! Reads an IOA TOML spec from stdin, runs the verification cascade, and writes
//! a JSON-encoded [`temper_verify::CascadeResult`] to stdout.
//!
//! Exit codes:
//! - **0** — all verification levels passed.
//! - **1** — one or more levels failed, or an error occurred.
//!
//! This subcommand is the subprocess endpoint for `temper serve --verify-subprocess`.
//! The parent process spawns `temper verify-ioa`, writes the IOA source to its stdin,
//! and reads the JSON result from its stdout.

use std::io::Read as _;

use anyhow::Result;

/// Run the `temper verify-ioa` subcommand.
///
/// Reads the full IOA TOML source from stdin and runs the verification cascade.
/// Writes the JSON-encoded result to stdout and exits 0 if all levels pass,
/// or exits 1 on failure.
pub fn run() -> Result<()> {
    let mut ioa_source = String::new();
    std::io::stdin()
        .read_to_string(&mut ioa_source)
        .map_err(|e| anyhow::anyhow!("failed to read IOA source from stdin: {e}"))?;

    let result = temper_verify::cascade::VerificationCascade::from_ioa(&ioa_source)
        .with_sim_seeds(5)
        .with_prop_test_cases(100)
        .run();

    let all_passed = result.all_passed;

    let json = serde_json::to_string(&result)
        .map_err(|e| anyhow::anyhow!("failed to serialize result: {e}"))?;
    println!("{json}");

    if !all_passed {
        std::process::exit(1);
    }

    Ok(())
}
