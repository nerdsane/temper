//! Generic HTTP adapter with fail-closed egress (ADR-0156 / ARN-228).

use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::egress::{
    ADAPTER_HTTP_TIMEOUT_SECS, ADAPTER_MAX_RESPONSE_BYTES, validate_adapter_http_url,
};
use super::{AdapterContext, AdapterError, AdapterResult, AgentAdapter};

/// Adapter implementation for generic HTTP callback execution.
#[derive(Debug, Default)]
pub struct HttpWebhookAdapter;

#[async_trait]
impl AgentAdapter for HttpWebhookAdapter {
    fn adapter_type(&self) -> &str {
        "http"
    }

    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        let started = Instant::now(); // determinism-ok: wall-clock timing for external HTTP

        let raw_url = ctx
            .integration_config
            .get("url")
            .or_else(|| ctx.integration_config.get("endpoint"))
            .cloned()
            .ok_or_else(|| {
                AdapterError::Invocation("missing adapter config key 'url'".to_string())
            })?;

        let url = validate_adapter_http_url(&raw_url).map_err(AdapterError::Invocation)?;

        let method = ctx
            .integration_config
            .get("method")
            .map(|m| m.to_ascii_uppercase())
            .unwrap_or_else(|| "POST".to_string());

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(ADAPTER_HTTP_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AdapterError::Invocation(format!("HTTP client build failed: {e}")))?;

        let mut request = client.request(
            reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| {
                AdapterError::Invocation(format!("invalid HTTP method '{method}': {e}"))
            })?,
            &url,
        );

        // Authorization material comes only from resolved integration config
        // templates — never from a full tenant secret map dump.
        if let Some(auth) = ctx.integration_config.get("authorization") {
            request = request.header("authorization", auth);
        }

        if let Some(token) = ctx.integration_config.get("bearer_token") {
            request = request.bearer_auth(token);
        }

        // Never forward ambient platform credentials or the full secret map.
        let payload = serde_json::json!({
            "tenant": ctx.tenant,
            "entity_type": ctx.entity_type,
            "entity_id": ctx.entity_id,
            "trigger_action": ctx.trigger_action,
            "trigger_params": ctx.trigger_params,
            "entity_state": ctx.entity_state,
            "agent_ctx": {
                "agent_id": ctx.agent_ctx.agent_id,
                "session_id": ctx.agent_ctx.session_id,
                "agent_type": ctx.agent_ctx.agent_type,
            },
        });

        let response = request
            .json(&payload)
            .send()
            .await
            .map_err(|e| AdapterError::Execution(format!("HTTP request failed: {e}")))?;

        let duration_ms = started.elapsed().as_millis() as u64;
        let status = response.status();

        // Fail closed before buffering: reject oversized Content-Length, then
        // stream with a hard byte cap (ARN-228 independent review P1).
        if let Some(cl) = response.content_length()
            && cl as usize > ADAPTER_MAX_RESPONSE_BYTES
        {
            return Ok(AdapterResult::failure(
                format!(
                    "HTTP Content-Length {cl} exceeds budget {ADAPTER_MAX_RESPONSE_BYTES}"
                ),
                duration_ms,
            ));
        }
        let mut collected = Vec::new();
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                AdapterError::Parse(format!("failed reading HTTP response body: {e}"))
            })?;
            if collected.len().saturating_add(chunk.len()) > ADAPTER_MAX_RESPONSE_BYTES {
                return Ok(AdapterResult::failure(
                    format!(
                        "HTTP response exceeded budget while streaming: > {ADAPTER_MAX_RESPONSE_BYTES}"
                    ),
                    duration_ms,
                ));
            }
            collected.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&collected).to_string();

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
        } else if status.is_redirection() {
            // Redirect policy is none; treat any 3xx as a blocked open-redirect surface.
            Ok(AdapterResult::failure(
                format!(
                    "HTTP {} returned redirect status {} (redirects are disabled)",
                    method, status
                ),
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
