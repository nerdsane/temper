//! Subprocess-based spec verification for isolation.
//!
//! When [`ServerState::verify_subprocess_bin`] is set, the verification cascade
//! runs in a child process rather than in the calling thread. This prevents
//! panics or infinite loops inside the verifier from crashing the main server.
//!
//! Protocol:
//! - Spawn `<bin> verify-ioa` with stdin/stdout piped.
//! - Write the IOA TOML source to stdin, then close it (signals EOF).
//! - Read the JSON-encoded [`temper_verify::CascadeResult`] from stdout.
//! - A 30-second wall-clock timeout is applied; the child is killed on expiry.

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

/// Timeout applied to each subprocess verification run.
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Run the verification cascade in an isolated subprocess.
///
/// Spawns `<bin> verify-ioa`, writes `ioa_source` to its stdin, and reads
/// a JSON-encoded [`temper_verify::CascadeResult`] from stdout.
///
/// Returns `Err(String)` if the subprocess fails to start, times out, exits
/// with a non-zero status, or produces output that cannot be parsed.
pub async fn verify_in_subprocess(
    bin: &Path,
    ioa_source: &str,
) -> Result<temper_verify::CascadeResult, String> {
    let mut child = tokio::process::Command::new(bin)
        .arg("verify-ioa")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn verification subprocess: {e}"))?;

    // Write IOA source to stdin and close the write end so the child sees EOF.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(ioa_source.as_bytes())
            .await
            .map_err(|e| format!("failed to write IOA source to subprocess stdin: {e}"))?;
        // `stdin` drops here, closing the pipe.
    }

    let output = tokio::time::timeout(SUBPROCESS_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "verification subprocess timed out after 30 seconds".to_string())?
        .map_err(|e| format!("verification subprocess I/O error: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "verification subprocess exited with {}: {stderr}",
            output.status
        ));
    }

    serde_json::from_slice::<temper_verify::CascadeResult>(&output.stdout)
        .map_err(|e| format!("failed to parse verification subprocess output: {e}"))
}
