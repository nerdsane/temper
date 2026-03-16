//! Generic HTTP adapter.

use std::net::IpAddr; // determinism-ok: IP parsing for SSRF prevention, no network I/O
use std::time::Instant;

use async_trait::async_trait;

use super::{AdapterContext, AdapterError, AdapterResult, AgentAdapter};

/// Check whether a URL targets a private/loopback address (SSRF prevention).
fn is_private_url(url: &str) -> bool {
    let domain = temper_wasm::authorized_host::extract_domain(url);
    if let Ok(ip) = domain.parse::<IpAddr>() {
        return ip.is_loopback() || is_private_ip(ip);
    }
    matches!(domain, "localhost" | "metadata.google.internal")
}

/// Returns true for RFC-1918, link-local, and cloud metadata IPs.
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()          // 127.0.0.0/8
            || v4.is_private()        // 10/8, 172.16/12, 192.168/16
            || v4.is_link_local()     // 169.254/16 (AWS metadata)
            || v4.octets()[0] == 0 // 0.0.0.0/8
        }
        IpAddr::V6(v6) => v6.is_loopback(), // ::1
    }
}

/// Adapter implementation for generic HTTP callback execution.
#[derive(Debug, Default)]
pub struct HttpWebhookAdapter;

#[async_trait]
impl AgentAdapter for HttpWebhookAdapter {
    fn adapter_type(&self) -> &str {
        "http"
    }

    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        let started = Instant::now(); // determinism-ok: wall-clock timing for adapter duration

        let url = ctx
            .integration_config
            .get("url")
            .or_else(|| ctx.integration_config.get("endpoint"))
            .cloned()
            .ok_or_else(|| {
                AdapterError::Invocation("missing adapter config key 'url'".to_string())
            })?;

        let allow_private = ctx
            .integration_config
            .get("allow_private_urls")
            .is_some_and(|v| v == "true");
        if !allow_private && is_private_url(&url) {
            return Err(AdapterError::Invocation(format!(
                "SSRF blocked: adapter URL targets private/loopback address: {url}"
            )));
        }

        let method = ctx
            .integration_config
            .get("method")
            .map(|m| m.to_ascii_uppercase())
            .unwrap_or_else(|| "POST".to_string());

        let mut request = reqwest::Client::new().request(
            reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| {
                AdapterError::Invocation(format!("invalid HTTP method '{method}': {e}"))
            })?,
            &url,
        );

        if let Some(auth) = ctx.integration_config.get("authorization") {
            request = request.header("authorization", auth);
        }

        if let Some(token) = ctx.integration_config.get("bearer_token") {
            request = request.bearer_auth(token);
        }

        let payload = serde_json::json!({
            "tenant": ctx.tenant,
            "entity_type": ctx.entity_type,
            "entity_id": ctx.entity_id,
            "trigger_action": ctx.trigger_action,
            "trigger_params": ctx.trigger_params,
            "entity_state": ctx.entity_state,
            "agent_ctx": ctx.agent_ctx,
        });

        let response = request
            .json(&payload)
            .send()
            .await
            .map_err(|e| AdapterError::Execution(format!("HTTP request failed: {e}")))?;

        let duration_ms = started.elapsed().as_millis() as u64;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| AdapterError::Parse(format!("failed reading HTTP response body: {e}")))?;

        if status.is_success() {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(params) = json.get("callback_params") {
                    return Ok(AdapterResult::success(params.clone(), duration_ms));
                }
                return Ok(AdapterResult::success(json, duration_ms));
            }
            Ok(AdapterResult::success(
                serde_json::json!({"response": text}),
                duration_ms,
            ))
        } else {
            Ok(AdapterResult::failure(
                format!("HTTP {} returned status {}: {}", method, status, text),
                duration_ms,
            ))
        }
    }
}
