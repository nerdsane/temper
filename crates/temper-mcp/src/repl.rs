//! HTTP REPL endpoint support for the Temper Monty sandbox.
//!
//! Exposes [`run_repl`] which runs Python code in the same Monty sandbox
//! used by the MCP server, with access to both `temper.*` (execute) and
//! `spec.*` (search) method dispatch. The Temper server mounts this behind
//! `POST /api/repl`.

use anyhow::Result;

use crate::runtime::RuntimeContext;

/// Configuration for a REPL session.
#[derive(Clone, Debug)]
pub struct ReplConfig {
    /// Port of the running Temper HTTP server.
    pub server_port: u16,
    /// Agent principal ID for `X-Temper-Principal-Id` header.
    pub principal_id: Option<String>,
}

/// Run Python code in the Temper Monty sandbox.
///
/// Creates a lightweight [`RuntimeContext`] configured to call back into the
/// server at `127.0.0.1:{port}`. All `temper.*` methods (create, action,
/// submit_specs, etc.) loop through HTTP to the local server.
///
/// Returns the result as a JSON string on success.
pub async fn run_repl(config: &ReplConfig, code: &str) -> Result<String> {
    let ctx = RuntimeContext::for_repl(config.server_port, config.principal_id.clone());
    ctx.run_execute(code).await
}
