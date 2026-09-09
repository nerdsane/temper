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

#[cfg(test)]
mod host_op_propagation_tests {
    use super::*;
    use std::sync::Arc;

    fn config(port: u16, allow_host_ops: bool) -> ReplConfig {
        // Return a valid credential so upload_wasm proceeds to the socket when
        // the gate is open; when the gate is closed it never mints one.
        let issuer: crate::http::InternalRequestCredentialIssuer = Arc::new(|_method, _url| {
            crate::http::InternalRequestCredential::new(
                "test-token".to_string(),
                "default".to_string(),
            )
        });
        ReplConfig {
            server_port: port,
            tenant: "default".to_string(),
            agent_id: None,
            session_id: None,
            internal_credential_issuer: issuer,
            allow_host_ops,
        }
    }

    /// Run `temper.upload_wasm` against a stand-in listener and report whether a
    /// connection reached it. The acceptor accepts then immediately drops the
    /// stream, so the REPL's HTTP client sees the peer close and errors out fast
    /// rather than blocking on a response the test never sends (no client
    /// timeout is configured). It signals each accepted connection on a channel,
    /// so the observation is event-driven — no fixed sleep, no shared counter.
    async fn upload_wasm_reaches_server(allow_host_ops: bool) -> bool {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(4);
        let acceptor = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
                if tx.send(()).await.is_err() {
                    break;
                }
            }
        });

        // Read a real, self-created file so the read cannot fail for reasons
        // unrelated to the gate; the file is portable (temp dir), unlike
        // /etc/hosts. With the gate open, upload_wasm reads it then POSTs.
        let wasm_path =
            std::env::temp_dir().join(format!("temper-arn166-{}.wasm", uuid::Uuid::new_v4()));
        tokio::fs::write(&wasm_path, b"\0asm\x01\0\0\0")
            .await
            .unwrap();
        let code = format!("temper.upload_wasm('m', '{}')", wasm_path.display());

        let _ = run_repl(&config(port, allow_host_ops), &code).await;
        let _ = tokio::fs::remove_file(&wasm_path).await;

        // A connection either already sits in the accept backlog (gate open) or
        // never comes (gate closed). Bound the wait so the closed case returns.
        let reached = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .ok()
            .flatten()
            .is_some();
        acceptor.abort();
        reached
    }

    /// End-to-end proof that config.allow_host_ops propagates through run_repl to
    /// the dispatch gate — closing the "flip the literal, tests still pass" gap.
    /// A connection to the stand-in server is the observable:
    ///   - allow_host_ops=false → the gate suppresses the whole operation → no connection.
    ///   - allow_host_ops=true  → upload_wasm reads its file and POSTs → a connection.
    /// The only variable between the two runs is the flag, so a connection on the
    /// `false` run (or none on `true`) means the flag failed to reach the gate.
    #[tokio::test]
    async fn allow_host_ops_propagates_from_config_to_the_dispatch_gate() {
        assert!(
            !upload_wasm_reaches_server(false).await,
            "with allow_host_ops=false the gate must suppress the operation before any server call"
        );
        assert!(
            upload_wasm_reaches_server(true).await,
            "with allow_host_ops=true the operation must proceed to the server — proving the flag, not a hardcoded block, governs the gate and that it propagates through run_repl"
        );
    }
}
