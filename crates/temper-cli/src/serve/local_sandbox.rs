//! The local development sandbox `temper serve` starts when no `SANDBOX_URL`
//! is configured.
//!
//! The sandbox runs commands and reads and writes files on this machine, so
//! only this server may use it: it listens on 127.0.0.1 alone and answers only
//! requests that carry the token generated here at each start. The token goes
//! to the sandbox in its environment (not its argv, which other local users
//! can list) and to the WASM host through the server state. Guests never see
//! it.

use std::path::Path;
use std::time::Duration;

use temper_server::state::ServerState;

/// Script that implements the sandbox HTTP API, relative to the checkout root.
const SANDBOX_SCRIPT: &str = "os-apps/temper-agent/sandbox/local_sandbox.py";

/// Environment variable through which the sandbox receives its token.
const SANDBOX_TOKEN_ENV: &str = "TEMPER_SANDBOX_TOKEN";

/// Start the local sandbox on `127.0.0.1:sandbox_port` and register it with
/// `server`, so WASM requests to `sandbox_url` carry its token.
///
/// The sandbox lives exactly as long as this process: it reads a pipe this
/// process holds open and exits at its end of input, so it stops however
/// the server stops, and the next start finds the port free. Startup does
/// not wait for it; [`supervise`] says when it is ready or that it exited.
pub(super) fn start(server: &ServerState, sandbox_port: u16, sandbox_url: &str) {
    let sandbox_script = Path::new(SANDBOX_SCRIPT);
    if !sandbox_script.exists() {
        eprintln!("  Warning: local sandbox script not found at {sandbox_script:?}");
        eprintln!("  Set SANDBOX_URL env var or ensure {SANDBOX_SCRIPT} exists");
        return;
    }

    // Use /tmp/temper-sandbox as the base; create /workspace for tool_runner
    // which sends cwd="/workspace" by default (matching E2B's layout).
    let _ = std::fs::create_dir_all("/tmp/temper-sandbox");
    let _ = std::fs::create_dir_all("/workspace");

    let token = new_token();
    let mut sandbox_cmd = tokio::process::Command::new("python3"); // determinism-ok: local dev sandbox
    sandbox_cmd
        .arg(sandbox_script)
        .arg("--port")
        .arg(sandbox_port.to_string())
        .arg("--workdir")
        .arg("/tmp/temper-sandbox")
        // Named explicitly so a script that cannot check the token refuses
        // the unknown flag and does not start.
        .arg("--token-env")
        .arg(SANDBOX_TOKEN_ENV)
        .arg("--exit-with-stdin")
        .env(SANDBOX_TOKEN_ENV, &token)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let started = server
        .set_local_sandbox(sandbox_url, token.clone())
        .and_then(|()| sandbox_cmd.spawn().map_err(|e| e.to_string()));
    match started {
        Ok(child) => {
            println!("  Local sandbox: {sandbox_url} (auto-started, not ready yet)");
            tokio::spawn(supervise(child, sandbox_url.to_string(), token));
        }
        Err(e) => {
            eprintln!("  Warning: failed to start local sandbox: {e}");
            eprintln!(
                "  Run manually: {SANDBOX_TOKEN_ENV}=<token> python3 {SANDBOX_SCRIPT} --port {sandbox_port}"
            );
        }
    }
}

/// Hold the sandbox for the life of this process and say when it is ready.
///
/// Its stdin pipe is held here (`wait` would close it, and the sandbox exits
/// when it closes). Readiness ends at whichever comes first: `/health`
/// answers with this start's token, or the sandbox exits, for example
/// because its port is taken. The exit also cancels a health request that
/// is still waiting for an answer.
async fn supervise(mut child: tokio::process::Child, sandbox_url: String, token: String) {
    let _stdin = child.stdin.take();
    tokio::select! {
        status = child.wait() => {
            tracing::warn!(
                status = ?status,
                sandbox_url,
                "local sandbox exited before it was ready; is its port already in use?"
            );
            return;
        }
        () = wait_for_health(&sandbox_url, &token) => {
            tracing::info!(sandbox_url, "local sandbox ready");
        }
    }
    let status = child.wait().await;
    tracing::warn!(status = ?status, sandbox_url, "local sandbox exited");
}

/// Return once the sandbox answers `/health` with `token`.
async fn wait_for_health(sandbox_url: &str, token: &str) {
    // Straight to loopback: a configured HTTP proxy must not see the token.
    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "cannot check the local sandbox; not reporting it ready");
            return std::future::pending().await;
        }
    };
    let health = format!("{sandbox_url}/health");
    loop {
        let answered = client.get(&health).bearer_auth(token).send().await;
        if answered.is_ok_and(|response| response.status().is_success()) {
            return;
        }
        // Not listening yet: python3 is still starting the script.
        tokio::time::sleep(Duration::from_millis(100)).await; // determinism-ok: local dev sandbox start
    }
}

/// A fresh 256-bit token, hex encoded.
fn new_token() -> String {
    // determinism-ok: OsRng used once at startup for the sandbox token
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
