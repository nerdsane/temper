//! HTTP helper functions for Temper server communication.
//!
//! Free async functions that take explicit parameters instead of being
//! methods on a context struct. This allows both MCP and agent-runtime
//! to use them with different context shapes.
//!
//! Identity is normally conveyed via `Authorization: Bearer` token.
//! The platform resolves the token to a verified agent identity (ADR-0033).

use std::sync::Arc;

use reqwest::Method;
use serde_json::Value;

use crate::helpers::{format_authz_denied, format_http_error};

/// One single-use credential for an exact internal HTTP request.
#[derive(Clone, Debug)]
pub struct InternalRequestCredential {
    bearer_token: String,
    tenant: String,
}

impl InternalRequestCredential {
    /// Construct a credential returned by a trusted server-side issuer.
    pub fn new(bearer_token: String, tenant: String) -> Result<Self, String> {
        if bearer_token.is_empty() || tenant.is_empty() {
            return Err("internal request credential fields must not be empty".to_string());
        }
        Ok(Self {
            bearer_token,
            tenant,
        })
    }
}

/// Callback that issues a fresh credential for one method and absolute URL.
pub type InternalRequestCredentialIssuer =
    Arc<dyn Fn(&str, &str) -> Result<InternalRequestCredential, String> + Send + Sync>;

/// Agent identity context attached to every Temper request.
///
/// Identity is conveyed via `Authorization: Bearer` token. The platform
/// resolves the token to a verified agent identity. Session grouping
/// is sent via `X-Session-Id`.
#[derive(Clone, Default)]
pub struct AgentIdentity<'a> {
    /// Session ID (becomes `X-Session-Id`).
    pub session_id: Option<&'a str>,
    /// Server-side issuer used by REPL loopback calls instead of raw identity.
    pub internal_credential_issuer: Option<&'a InternalRequestCredentialIssuer>,
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

/// Send an HTTP request to the Temper server.
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
        http, base_url, tenant, identity, api_key, method, path, body,
    )
    .await
}

/// Send a governance request (admin) to the Temper server.
///
/// Uses the same Bearer token authentication. The platform determines
/// admin privileges from the credential, not from headers.
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
        http, base_url, tenant, identity, api_key, method, path, body,
    )
    .await
}

/// Core JSON request sender.
#[allow(clippy::too_many_arguments)]
async fn send_json(
    http: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    identity: &AgentIdentity<'_>,
    api_key: Option<&str>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let url = format!("{base_url}{path}");
    let internal_credential = identity
        .internal_credential_issuer
        .map(|issuer| issuer(method.as_str(), &url))
        .transpose()?;
    let request_tenant = internal_credential
        .as_ref()
        .map_or(tenant, |credential| credential.tenant.as_str());
    let mut request = http
        .request(method, &url)
        .header("X-Tenant-Id", request_tenant)
        .header("Accept", "application/json");

    if let Some(sid) = identity.session_id {
        request = request.header("X-Session-Id", sid);
    }
    if let Some(credential) = internal_credential.as_ref() {
        request = request.header(
            "Authorization",
            format!("Bearer {}", credential.bearer_token),
        );
    } else if let Some(key) = api_key {
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
    let internal_credential = identity
        .internal_credential_issuer
        .map(|issuer| issuer(method.as_str(), &url))
        .transpose()?;
    let request_tenant = internal_credential
        .as_ref()
        .map_or(tenant, |credential| credential.tenant.as_str());
    let mut request = http
        .request(method, &url)
        .header("X-Tenant-Id", request_tenant)
        .header("Content-Type", "application/wasm");

    if let Some(credential) = internal_credential.as_ref() {
        request = request.header(
            "Authorization",
            format!("Bearer {}", credential.bearer_token),
        );
    } else if let Some(key) = api_key {
        request = request.header("Authorization", format!("Bearer {key}"));
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
