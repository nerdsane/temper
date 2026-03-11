//! HTTP helper functions for Temper server communication.
//!
//! Free async functions that take explicit parameters instead of being
//! methods on a context struct. This allows both MCP and agent-runtime
//! to use them with different context shapes.

use reqwest::Method;
use serde_json::Value;

use crate::helpers::{format_authz_denied, format_http_error};

/// Agent identity triple attached to every Temper request.
///
/// Sent as HTTP headers:
/// - `X-Temper-Principal-Id` — unique agent instance ID
/// - `X-Temper-Agent-Type` — agent software classification (e.g. `claude-code`)
/// - `X-Session-Id` — session grouping
#[derive(Debug, Clone, Default)]
pub struct AgentIdentity<'a> {
    /// Agent instance ID (becomes `X-Temper-Principal-Id`).
    pub agent_id: Option<&'a str>,
    /// Agent software type (becomes `X-Temper-Agent-Type`).
    pub agent_type: Option<&'a str>,
    /// Session ID (becomes `X-Session-Id`).
    pub session_id: Option<&'a str>,
}

/// Principal kind to attach to a Temper request.
enum PrincipalKind {
    /// Agent principal — standard API caller.
    Agent,
    /// Admin/governance principal — elevated privileges.
    Admin,
}

/// Process a Temper HTTP response into a `Result<Value, String>`.
///
/// Handles success (including empty bodies), authz denials (403 with
/// structured body), and generic HTTP errors.
async fn process_response(response: reqwest::Response, url: &str) -> Result<Value, String> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read Temper response body from {url}: {e}"))?;

    if status.is_success() {
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&text).or(Ok(Value::String(text)));
    }

    if status == reqwest::StatusCode::FORBIDDEN
        && let Some(structured) = format_authz_denied(&text)
    {
        return Ok(structured);
    }

    Err(format_http_error(status, &text))
}

/// Send an HTTP request to the Temper server with agent principal headers.
#[allow(clippy::too_many_arguments)]
pub async fn temper_request(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    identity: &AgentIdentity<'_>,
    api_key: Option<&str>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    send_json(
        http,
        base_url,
        tenant,
        PrincipalKind::Agent,
        identity,
        api_key,
        method,
        path,
        body,
    )
    .await
}

/// Send a governance request (admin principal) to the Temper server.
#[allow(clippy::too_many_arguments)]
pub async fn temper_governance_request(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    identity: &AgentIdentity<'_>,
    api_key: Option<&str>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    send_json(
        http,
        base_url,
        tenant,
        PrincipalKind::Admin,
        identity,
        api_key,
        method,
        path,
        body,
    )
    .await
}

/// Core JSON request sender.
#[allow(clippy::too_many_arguments)]
async fn send_json(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    kind: PrincipalKind,
    identity: &AgentIdentity<'_>,
    api_key: Option<&str>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let url = format!("{base_url}{path}");
    let mut request = http
        .request(method, &url)
        .header("X-Tenant-Id", tenant)
        .header("Accept", "application/json");

    match kind {
        PrincipalKind::Agent => {
            if let Some(pid) = identity.agent_id {
                request = request
                    .header("X-Temper-Principal-Kind", "agent")
                    .header("X-Temper-Principal-Id", pid);
            }
            // No explicit principal — omit headers entirely so the server
            // applies its own default (Customer) rather than granting admin.
        }
        PrincipalKind::Admin => {
            let admin_id = identity.agent_id.unwrap_or("governance-admin");
            request = request
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Temper-Principal-Id", admin_id);
        }
    }

    if let Some(at) = identity.agent_type {
        request = request.header("X-Temper-Agent-Type", at);
    }
    if let Some(sid) = identity.session_id {
        request = request.header("X-Session-Id", sid);
    }

    if let Some(key) = api_key {
        request = request.header("Authorization", format!("Bearer {key}"));
    }

    if let Some(payload) = body {
        request = request.json(payload);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

    process_response(response, &url).await
}

/// Send a request with a raw binary body (e.g. WASM module bytes).
#[allow(clippy::too_many_arguments)]
pub async fn temper_request_bytes(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    identity: &AgentIdentity<'_>,
    api_key: Option<&str>,
    method: Method,
    path: &str,
    body: Vec<u8>,
) -> Result<Value, String> {
    let url = format!("{base_url}{path}");
    let mut request = http
        .request(method, &url)
        .header("X-Tenant-Id", tenant)
        .header("Content-Type", "application/wasm");

    if let Some(key) = api_key {
        request = request.header("Authorization", format!("Bearer {key}"));
    }

    if let Some(pid) = identity.agent_id {
        request = request
            .header("X-Temper-Principal-Kind", "agent")
            .header("X-Temper-Principal-Id", pid);
    }
    // No explicit principal — omit headers entirely so the server
    // applies its own default (Customer) rather than granting admin.

    if let Some(at) = identity.agent_type {
        request = request.header("X-Temper-Agent-Type", at);
    }
    if let Some(sid) = identity.session_id {
        request = request.header("X-Session-Id", sid);
    }

    request = request.body(body);

    let response = request
        .send()
        .await
        .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

    process_response(response, &url).await
}
