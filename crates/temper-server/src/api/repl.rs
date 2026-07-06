//! REPL sandbox API endpoint.
//!
//! Exposes the Temper Monty sandbox over HTTP, allowing agents to execute
//! Python code with `temper.*` methods that loop back to the server.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::odata::extract_tenant;
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

/// Request body for POST /api/repl.
#[derive(serde::Deserialize)]
pub(crate) struct ReplRequest {
    code: String,
}

/// POST /api/repl — execute Python code in the Temper Monty sandbox.
///
/// The sandbox provides `temper.*` methods (create, action, submit_specs, etc.)
/// that loop back to this server over HTTP; each loopback call is subject to the
/// server's normal Cedar authorization. Agent identity is taken from
/// `X-Temper-Principal-Id` / `X-Temper-Principal-Kind` / `X-Temper-Agent-Role`
/// headers and forwarded on those internal requests.
///
/// Executing the REPL requires Cedar authorization (`execute_repl` on
/// `Sandbox`). Host operations — local filesystem reads and `cargo build`
/// (`upload_wasm`/`compile_wasm`) — are NOT available here: this dispatch
/// context is not host-trusted, so those methods are rejected before touching
/// the host. They run only on the developer's own machine via the local MCP
/// server. Resource bounds: 180s timeout, 64MB memory.
#[instrument(skip_all, fields(otel.name = "POST /api/repl"))]
pub(crate) async fn handle_repl(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ReplRequest>,
) -> impl IntoResponse {
    let principal_id = headers
        .get("x-temper-principal-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let principal_kind = headers
        .get("x-temper-principal-kind")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let agent_role = headers
        .get("x-temper-agent-role")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let agent_type = headers
        .get("x-temper-agent-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let session_id = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t.as_str().to_string(),
        Err(e) => return e.into_response(),
    };

    // Executing the REPL is a privileged capability (arbitrary agent code that
    // can drive every non-host temper.* method). Gate it behind Cedar before
    // running anything.
    if let Some(resp) = super::require_repl_auth(&state, &headers, &tenant).await {
        return resp;
    }

    let agent_id = principal_id.clone();
    let port = state.listen_port.get().copied().unwrap_or(4200);
    let code = body.code;
    let tenant_for_repl = tenant.clone();

    // The Monty sandbox types are !Send, so we run in a dedicated
    // single-threaded runtime via spawn_blocking.
    let result = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create REPL runtime"); // determinism-ok: one-shot runtime for sandbox
        rt.block_on(async move {
            let config = temper_sandbox::repl::ReplConfig {
                server_port: port,
                tenant: tenant_for_repl,
                agent_id: principal_id.clone(),
                principal_id,
                principal_kind,
                agent_role,
                agent_type,
                session_id,
            };
            temper_sandbox::repl::run_repl(&config, &code).await
        })
    })
    .await;

    match result {
        Ok(Ok(result_json)) => {
            let value: serde_json::Value = serde_json::from_str(&result_json)
                .unwrap_or(serde_json::Value::String(result_json));
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "result": value,
                    "error": serde_json::Value::Null,
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "REPL sandbox execution error");
            // Record sandbox error as trajectory entry (unmet intent).
            let entry = TrajectoryEntry {
                timestamp: sim_now().to_rfc3339(),
                tenant: tenant.clone(),
                entity_type: "sandbox".to_string(),
                entity_id: String::new(),
                action: "repl_execution".to_string(),
                success: false,
                from_status: None,
                to_status: None,
                error: Some(e.to_string()),
                agent_id: agent_id.clone(),
                session_id: None,
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some(TrajectorySource::Platform),
                spec_governed: None,
                agent_type: None,
                request_body: None,
                intent: None,
                matched_policy_ids: None,
            };
            if !state.enqueue_trajectory_entry(entry) {
                tracing::warn!("failed to enqueue REPL trajectory entry");
            }

            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "result": serde_json::Value::Null,
                    "error": e.to_string(),
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "REPL task panicked");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "result": serde_json::Value::Null,
                    "error": format!("REPL task panicked: {e}"),
                })),
            )
                .into_response()
        }
    }
}
