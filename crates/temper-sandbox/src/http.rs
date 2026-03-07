//! HTTP helper functions for Temper server communication.
//!
//! Free async functions that take explicit parameters instead of being
//! methods on a context struct. This allows both MCP and agent-runtime
//! to use them with different context shapes.

use reqwest::Method;
use serde_json::Value;

use crate::helpers::{format_authz_denied, format_http_error};

/// Principal identity to attach to a Temper request.
pub enum Principal<'a> {
    /// Agent principal — standard API caller.
    Agent(Option<&'a str>),
    /// Admin/governance principal — elevated privileges.
    Admin(Option<&'a str>),
}

/// Process a Temper HTTP response into a `Result<Value, String>`.
///
/// Handles success (including empty bodies), authz denials (403 with
/// structured body), and generic HTTP errors.
async fn process_response(
    response: reqwest::Response,
    url: &str,
) -> Result<Value, String> {
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
pub async fn temper_request(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    principal_id: Option<&str>,
    api_key: Option<&str>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    send_json(http, base_url, tenant, Principal::Agent(principal_id), api_key, method, path, body).await
}

/// Send a governance request (admin principal) to the Temper server.
pub async fn temper_governance_request(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    principal_id: Option<&str>,
    api_key: Option<&str>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    send_json(http, base_url, tenant, Principal::Admin(principal_id), api_key, method, path, body).await
}

/// Core JSON request sender shared by `temper_request` and `temper_governance_request`.
async fn send_json(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    principal: Principal<'_>,
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

    match principal {
        Principal::Agent(Some(pid)) => {
            request = request
                .header("X-Temper-Principal-Kind", "agent")
                .header("X-Temper-Principal-Id", pid);
        }
        Principal::Agent(None) => {
            // No explicit principal — omit headers entirely so the server
            // applies its own default (Customer) rather than granting admin.
        }
        Principal::Admin(pid) => {
            let admin_id = pid.unwrap_or("governance-admin");
            request = request
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Temper-Principal-Id", admin_id);
        }
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
pub async fn temper_request_bytes(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    principal_id: Option<&str>,
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

    if let Some(pid) = principal_id {
        request = request
            .header("X-Temper-Principal-Kind", "agent")
            .header("X-Temper-Principal-Id", pid);
    }
    // No explicit principal — omit headers entirely so the server
    // applies its own default (Customer) rather than granting admin.

    request = request.body(body);

    let response = request
        .send()
        .await
        .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

    process_response(response, &url).await
}
