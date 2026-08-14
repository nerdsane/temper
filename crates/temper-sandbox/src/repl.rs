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
#[derive(Clone)]
pub struct ReplConfig {
    /// Port of the running Temper HTTP server.
    pub server_port: u16,
    /// Tenant whose local HTTP API should receive loopback calls.
    pub tenant: String,
    /// Optional local label for the REPL session.
    pub agent_id: Option<String>,
    /// Session id forwarded from the REPL request boundary.
    pub session_id: Option<String>,
    /// Per-request credential issuer for authenticated loopback calls.
    pub internal_credential_issuer: crate::http::InternalRequestCredentialIssuer,
    /// Whether host-process ops (`upload_wasm`/`compile_wasm`) are permitted.
    ///
    /// The server-hosted REPL sets this false: those ops would read the server's
    /// filesystem and spawn `cargo` as the server user (ARN-166). The local
    /// stdio MCP runner — the developer's own machine — sets it true.
    pub allow_host_ops: bool,
}

/// Run Python code in the Temper Monty sandbox via the REPL endpoint.
///
/// Creates a lightweight HTTP client and dispatches `temper.*` methods
/// back to the server at `127.0.0.1:{port}`.
pub async fn run_repl(config: &ReplConfig, code: &str) -> Result<String> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let base_url = format!("http://127.0.0.1:{}", config.server_port);
    let tenant = config.tenant.clone();
    let agent_id = config.agent_id.clone();
    let session_id = config.session_id.clone();
    let internal_credential_issuer = config.internal_credential_issuer.clone();
    let allow_host_ops = config.allow_host_ops;

    run_sandbox(
        code,
        "repl.py",
        &[("temper", "Temper", 1)],
        |function_name: String, args: Vec<MontyObject>, kwargs: Vec<(MontyObject, MontyObject)>| {
            let http = http.clone();
            let base_url = base_url.clone();
            let tenant = tenant.clone();
            let agent_id = agent_id.clone();
            let session_id = session_id.clone();
            let internal_credential_issuer = internal_credential_issuer.clone();
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
                    tenant: &tenant,
                    agent_id: agent_id.as_deref(),
                    session_id: session_id.as_deref(),
                    entity_set_resolver: None,
                    binary_path: None,
                    api_key: None,
                    internal_credential_issuer: Some(&internal_credential_issuer),
                    allow_host_ops,
                };
                dispatch_temper_method(&ctx, &function_name, args, &kwargs).await
            }
        },
    )
    .await
}
