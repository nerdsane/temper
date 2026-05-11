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
    /// Tenant whose local HTTP API should receive loopback calls.
    pub tenant: String,
    /// Optional local label for the REPL session.
    pub agent_id: Option<String>,
    /// Caller principal id forwarded from the REPL request boundary.
    pub principal_id: Option<String>,
    /// Caller principal kind forwarded from the REPL request boundary.
    pub principal_kind: Option<String>,
    /// Agent role forwarded from the REPL request boundary.
    pub agent_role: Option<String>,
    /// Agent type forwarded from the REPL request boundary.
    pub agent_type: Option<String>,
    /// Session id forwarded from the REPL request boundary.
    pub session_id: Option<String>,
}

/// Run Python code in the Temper Monty sandbox via the REPL endpoint.
///
/// Creates a lightweight HTTP client and dispatches `temper.*` methods
/// back to the server at `127.0.0.1:{port}`.
pub async fn run_repl(config: &ReplConfig, code: &str) -> Result<String> {
    let http = reqwest::Client::new();
    let base_url = format!("http://127.0.0.1:{}", config.server_port);
    let tenant = config.tenant.clone();
    let agent_id = config.agent_id.clone();
    let principal_id = config.principal_id.clone();
    let principal_kind = config.principal_kind.clone();
    let agent_role = config.agent_role.clone();
    let agent_type = config.agent_type.clone();
    let session_id = config.session_id.clone();

    run_sandbox(
        code,
        "repl.py",
        &[("temper", "Temper", 1)],
        |function_name: String, args: Vec<MontyObject>, kwargs: Vec<(MontyObject, MontyObject)>| {
            let http = http.clone();
            let base_url = base_url.clone();
            let tenant = tenant.clone();
            let agent_id = agent_id.clone();
            let principal_id = principal_id.clone();
            let principal_kind = principal_kind.clone();
            let agent_role = agent_role.clone();
            let agent_type = agent_type.clone();
            let session_id = session_id.clone();
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
                    agent_type: agent_type.as_deref(),
                    session_id: session_id.as_deref(),
                    principal_id: principal_id.as_deref(),
                    principal_kind: principal_kind.as_deref(),
                    agent_role: agent_role.as_deref(),
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
