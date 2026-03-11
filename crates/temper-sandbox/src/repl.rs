//! HTTP REPL endpoint support for the Temper Monty sandbox.
//!
//! Exposes [`run_repl`] which runs Python code in the Monty sandbox
//! with access to `temper.*` methods. The Temper server mounts this
//! behind `POST /api/repl`.

use anyhow::Result;
use monty::MontyObject;

use crate::dispatch::{DispatchContext, dispatch_temper_method};
use crate::runner::run_sandbox;

/// Configuration for a REPL session.
#[derive(Clone, Debug)]
pub struct ReplConfig {
    /// Port of the running Temper HTTP server.
    pub server_port: u16,
    /// Agent ID for `X-Temper-Principal-Id` header.
    pub agent_id: Option<String>,
}

/// Run Python code in the Temper Monty sandbox via the REPL endpoint.
///
/// Creates a lightweight HTTP client and dispatches `temper.*` methods
/// back to the server at `127.0.0.1:{port}`.
pub async fn run_repl(config: &ReplConfig, code: &str) -> Result<String> {
    let http = reqwest::Client::new();
    let base_url = format!("http://127.0.0.1:{}", config.server_port);
    let agent_id = config.agent_id.clone();

    run_sandbox(
        code,
        "repl.py",
        &[("temper", "Temper", 1)],
        |function_name: String, args: Vec<MontyObject>, kwargs: Vec<(MontyObject, MontyObject)>| {
            let http = http.clone();
            let base_url = base_url.clone();
            let agent_id = agent_id.clone();
            async move {
                // Strip self arg (dataclass method calls include self as args[0])
                let args = if args.is_empty() {
                    &args[..]
                } else {
                    &args[1..]
                };
                let ctx = DispatchContext {
                    http: &http,
                    base_url: &base_url,
                    tenant: "default",
                    agent_id: agent_id.as_deref(),
                    agent_type: None,
                    session_id: None,
                    entity_set_resolver: None,
                    binary_path: None,
                    api_key: None,
                };
                dispatch_temper_method(&ctx, &function_name, args, &kwargs).await
            }
        },
    )
    .await
}
