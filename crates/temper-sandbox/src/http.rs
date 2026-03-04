//! HTTP helper functions for Temper server communication.
//!
//! Free async functions that take explicit parameters instead of being
//! methods on a context struct. This allows both MCP and agent-runtime
//! to use them with different context shapes.

use reqwest::Method;
use serde_json::Value;

use crate::helpers::{format_authz_denied, format_http_error};

/// Send an HTTP request to the Temper server with agent principal headers.
pub async fn temper_request(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    principal_id: Option<&str>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let url = format!("{base_url}{path}");
    let mut request = http
        .request(method, &url)
        .header("X-Tenant-Id", tenant)
        .header("Accept", "application/json");

    if let Some(pid) = principal_id {
        request = request
            .header("X-Temper-Principal-Kind", "agent")
            .header("X-Temper-Principal-Id", pid);
    }

    if let Some(payload) = body {
        request = request.json(payload);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read Temper response body: {e}"))?;

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

/// Send a governance request (admin principal) to the Temper server.
pub async fn temper_governance_request(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    principal_id: Option<&str>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let url = format!("{base_url}{path}");
    let admin_id = principal_id.unwrap_or("governance-admin");
    let mut request = http
        .request(method, &url)
        .header("X-Tenant-Id", tenant)
        .header("Accept", "application/json")
        .header("X-Temper-Principal-Kind", "admin")
        .header("X-Temper-Principal-Id", admin_id);

    if let Some(payload) = body {
        request = request.json(payload);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read Temper response body: {e}"))?;

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

/// Send a request with a raw binary body (e.g. WASM module bytes).
pub async fn temper_request_bytes(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    principal_id: Option<&str>,
    method: Method,
    path: &str,
    body: Vec<u8>,
) -> Result<Value, String> {
    let url = format!("{base_url}{path}");
    let mut request = http
        .request(method, &url)
        .header("X-Tenant-Id", tenant)
        .header("Content-Type", "application/wasm");

    if let Some(pid) = principal_id {
        request = request
            .header("X-Temper-Principal-Kind", "agent")
            .header("X-Temper-Principal-Id", pid);
    }

    request = request.body(body);

    let response = request
        .send()
        .await
        .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read Temper response body: {e}"))?;

    if status.is_success() {
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&text).or(Ok(Value::String(text)));
    }

    Err(format_http_error(status, &text))
}
