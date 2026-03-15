//! Generic HTTP adapter.

use std::time::Instant;

use async_trait::async_trait;

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
        let started = Instant::now();

        let url = ctx
            .integration_config
            .get("url")
            .or_else(|| ctx.integration_config.get("endpoint"))
            .cloned()
            .ok_or_else(|| {
                AdapterError::Invocation("missing adapter config key 'url'".to_string())
            })?;

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
