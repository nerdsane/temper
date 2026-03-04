//! HTTP REPL endpoint support for the Temper Monty sandbox.
//!
//! Exposes [`run_repl`] which runs Python code in the Monty sandbox
//! with access to `temper.*` methods. The Temper server mounts this
//! behind `POST /api/repl`.

use anyhow::Result;
use monty::MontyObject;

use crate::dispatch::dispatch_temper_method;
use crate::runner::run_sandbox;

/// Configuration for a REPL session.
#[derive(Clone, Debug)]
pub struct ReplConfig {
    /// Port of the running Temper HTTP server.
    pub server_port: u16,
    /// Agent principal ID for `X-Temper-Principal-Id` header.
    pub principal_id: Option<String>,
}

/// Run Python code in the Temper Monty sandbox via the REPL endpoint.
///
/// Creates a lightweight HTTP client and dispatches `temper.*` methods
/// back to the server at `127.0.0.1:{port}`.
pub async fn run_repl(config: &ReplConfig, code: &str) -> Result<String> {
    let http = reqwest::Client::new();
    let base_url = format!("http://127.0.0.1:{}", config.server_port);
    let principal_id = config.principal_id.clone();

    run_sandbox(
        code,
        "repl.py",
        &[("temper", "Temper", 1)],
        |function_name: String, args: Vec<MontyObject>, kwargs: Vec<(MontyObject, MontyObject)>| {
            let http = http.clone();
            let base_url = base_url.clone();
            let principal_id = principal_id.clone();
            async move {
                // Strip self arg (dataclass method calls include self as args[0])
                let args = if args.is_empty() { &args[..] } else { &args[1..] };
                dispatch_temper_method(
                    &http,
                    &base_url,
                    "default",
                    principal_id.as_deref(),
                    &function_name,
                    args,
                    &kwargs,
                    None,
                    None,
                )
                .await
            }
        },
    )
    .await
}
