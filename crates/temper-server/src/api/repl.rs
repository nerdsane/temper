//! REPL sandbox API endpoint.
//!
//! Exposes the Temper Monty sandbox over HTTP, allowing agents to execute
//! Python code with `temper.*` methods that loop back to the server.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::authz::require_authenticated_context;
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

fn require_repl_authorization(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
) -> Result<(), StatusCode> {
    state
        .authorize_with_context(
            authenticated.security_context(),
            "execute_repl",
            "Repl",
            &std::collections::BTreeMap::from([(
                "id".to_string(),
                serde_json::Value::String(authenticated.tenant().to_string()),
            )]),
            authenticated.tenant().as_str(),
        )
        .map_err(|denial| {
            tracing::warn!(
                reason = %denial,
                tenant = %authenticated.tenant(),
                principal_id = %authenticated.security_context().principal.id,
                "REPL execution denied"
            );
            StatusCode::FORBIDDEN
        })
}

/// Request body for POST /api/repl.
#[derive(serde::Deserialize)]
pub(crate) struct ReplRequest {
    code: String,
}

/// POST /api/repl — execute Python code in the Temper Monty sandbox.
///
/// The sandbox provides `temper.*` methods (create, action, submit_specs, etc.)
/// that loop back to this server via single-use, request-bound credentials.
///
/// Security: 180s timeout, 64MB memory, method allowlisting, no filesystem or
/// network access. External APIs go through `[[integration]]` in IOA specs.
#[instrument(skip_all, fields(otel.name = "POST /api/repl"))]
/// Build the `ReplConfig` for the server-hosted REPL.
///
/// `allow_host_ops` is hardcoded false here and only here: the server-hosted
/// REPL's host process is the Temper server, so `upload_wasm`/`compile_wasm`
/// would read server files and spawn `cargo` as the server user (ARN-166,
/// ADR-0158). Host ops belong to the local MCP server on the developer's own
/// machine. Centralised in one constructor so the invariant has a single tested
/// home rather than an inline literal that a refactor could flip unnoticed.
fn server_repl_config(
    server_port: u16,
    tenant: String,
    agent_id: String,
    session_id: Option<String>,
    internal_credential_issuer: temper_sandbox::http::InternalRequestCredentialIssuer,
) -> temper_sandbox::repl::ReplConfig {
    temper_sandbox::repl::ReplConfig {
        server_port,
        tenant,
        agent_id: Some(agent_id),
        session_id,
        internal_credential_issuer,
        allow_host_ops: false,
    }
}

pub(crate) async fn handle_repl(
    State(state): State<ServerState>,
    headers: HeaderMap,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(body): axum::Json<ReplRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    if let Err(status) = require_repl_authorization(&state, authenticated) {
        return status.into_response();
    }
    // Session and intent come from the canonical observability extractor so the
    // REPL honours the same header aliases as every other entrypoint
    // (`X-Temper-Observe-Session-Id`/`X-Session-Id`,
    // `X-Temper-Observe-Intent`/`X-Intent`). These are correlation-only and
    // carry no authority; identity comes from the credential above.
    let agent_ctx = crate::request_context::extract_agent_context(&headers);
    let session_id = agent_ctx.session_id.clone();

    let tenant = authenticated.tenant().to_string();
    let agent_type = authenticated
        .security_context()
        .principal
        .agent_type
        .clone();
    let Some(capability_issuer) = crate::state::internal_http_capability_issuer(
        &state,
        authenticated.tenant(),
        Some(authenticated.security_context()),
    ) else {
        tracing::error!("authenticated REPL request has no internal capability issuer");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let internal_credential_issuer: temper_sandbox::http::InternalRequestCredentialIssuer =
        Arc::new(move |method, url| {
            let capability = capability_issuer(method, url)?;
            temper_sandbox::http::InternalRequestCredential::new(
                capability.bearer_token().to_string(),
                capability.tenant().to_string(),
            )
        });

    let agent_id = authenticated.security_context().principal.id.clone();
    let trajectory_agent_type = agent_type.clone();
    let port = state.listen_port.get().copied().unwrap_or(4200);
    let code = body.code;
    // The submitted code is the request body of a REPL call; a failed run is
    // unreadable without it. Size is bounded when the entry is enqueued.
    let submitted_code = code.clone();
    let tenant_for_repl = tenant.clone();
    let agent_id_for_repl = agent_id.clone();

    // The Monty sandbox types are !Send, so we run in a dedicated
    // single-threaded runtime via spawn_blocking.
    let result = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create REPL runtime"); // determinism-ok: one-shot runtime for sandbox
        rt.block_on(async move {
            let config = server_repl_config(
                port,
                tenant_for_repl,
                agent_id_for_repl,
                session_id,
                internal_credential_issuer,
            );
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
                agent_id: Some(agent_id.clone()),
                session_id: agent_ctx.session_id.clone(),
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some(TrajectorySource::Platform),
                spec_governed: None,
                agent_type: trajectory_agent_type.clone(),
                request_body: Some(serde_json::json!({ "code": submitted_code })),
                intent: agent_ctx.intent.clone(),
                matched_policy_ids: None,
                capture_seq: None,
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

#[cfg(test)]
mod tests {
    use temper_authz::{AuthenticatedRequestContext, SecurityContext};
    use temper_runtime::ActorSystem;
    use temper_runtime::tenant::TenantId;

    use super::require_repl_authorization;
    use crate::registry::SpecRegistry;
    use crate::state::ServerState;

    use super::server_repl_config;

    /// The server-hosted REPL must never permit host ops (ARN-166). This guards
    /// the exact production constructor, so a future flip of the `allow_host_ops`
    /// literal is caught here rather than reopening the RCE silently.
    #[test]
    fn server_repl_config_never_allows_host_ops() {
        let issuer: temper_sandbox::http::InternalRequestCredentialIssuer =
            std::sync::Arc::new(|_method, _url| Err("unused".to_string()));
        let config = server_repl_config(
            3000,
            "tenant-a".to_string(),
            "agent-1".to_string(),
            Some("session-1".to_string()),
            issuer,
        );
        assert!(
            !config.allow_host_ops,
            "the server-hosted REPL must not permit host-process ops"
        );
    }

    #[test]
    fn repl_requires_explicit_tenant_resource_authority() {
        let state = ServerState::from_registry(ActorSystem::new("repl-auth"), SpecRegistry::new());
        state
            .authz
            .reload_tenant_policies(
                "tenant-a",
                r#"
permit(
  principal == Agent::"agent-1",
  action == Action::"execute_repl",
  resource == Repl::"tenant-a"
);
"#,
            )
            .expect("REPL policy should parse");
        let allowed = AuthenticatedRequestContext::new(
            TenantId::new("tenant-a"),
            SecurityContext::from_resolved_identity("agent-1", "worker", None),
        );
        let denied = AuthenticatedRequestContext::new(
            TenantId::new("tenant-a"),
            SecurityContext::from_resolved_identity("agent-2", "worker", None),
        );
        let claimed_admin = AuthenticatedRequestContext::new(
            TenantId::new("tenant-a"),
            SecurityContext {
                principal: temper_authz::Principal {
                    id: "claimed-admin".to_string(),
                    kind: temper_authz::PrincipalKind::Admin,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "repl-admin-side-channel-test".to_string(),
            },
        );

        assert!(require_repl_authorization(&state, &allowed).is_ok());
        assert_eq!(
            require_repl_authorization(&state, &denied),
            Err(axum::http::StatusCode::FORBIDDEN)
        );
        assert_eq!(
            require_repl_authorization(&state, &claimed_admin),
            Err(axum::http::StatusCode::FORBIDDEN)
        );
    }
}
