//! OData HTTP client for the agent to interact with the Temper server.

use serde_json::Value;

/// HTTP client for OData operations against a Temper server.
pub struct TemperClient {
    base_url: String,
    http: reqwest::Client,
    agent_id: String,
}

impl TemperClient {
    /// Create a new client targeting the given base URL.
    pub fn new(base_url: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
            agent_id: agent_id.into(),
        }
    }

    /// GET an entity by set name and key.
    pub async fn get_entity(&self, entity_set: &str, id: &str) -> Result<Value, String> {
        let url = format!("{}/odata/{}('{}')", self.base_url, entity_set, id);
        let resp = self.http.get(&url)
            .header("X-Temper-Principal-Id", &self.agent_id)
            .header("X-Temper-Principal-Kind", "agent")
            .header("X-Temper-Agent-Role", "customer_agent")
            .send().await
            .map_err(|e| format!("HTTP error: {e}"))?;

        resp.json::<Value>().await.map_err(|e| format!("JSON error: {e}"))
    }

    /// POST to create a new entity.
    pub async fn create_entity(&self, entity_set: &str) -> Result<Value, String> {
        let url = format!("{}/odata/{}", self.base_url, entity_set);
        let resp = self.http.post(&url)
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Id", &self.agent_id)
            .header("X-Temper-Principal-Kind", "agent")
            .header("X-Temper-Agent-Role", "customer_agent")
            .body("{}")
            .send().await
            .map_err(|e| format!("HTTP error: {e}"))?;

        resp.json::<Value>().await.map_err(|e| format!("JSON error: {e}"))
    }

    /// POST to invoke a bound action on an entity.
    pub async fn invoke_action(
        &self,
        entity_set: &str,
        id: &str,
        action: &str,
        params: &Value,
    ) -> Result<Value, String> {
        let url = format!(
            "{}/odata/{}('{}')/Temper.Ecommerce.{}",
            self.base_url, entity_set, id, action
        );
        let resp = self.http.post(&url)
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Id", &self.agent_id)
            .header("X-Temper-Principal-Kind", "agent")
            .header("X-Temper-Agent-Role", "customer_agent")
            .json(params)
            .send().await
            .map_err(|e| format!("HTTP error: {e}"))?;

        let status = resp.status();
        let body = resp.json::<Value>().await.map_err(|e| format!("JSON error: {e}"))?;

        if status.is_success() {
            Ok(body)
        } else {
            let err_msg = body.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown error");
            Err(format!("Action failed ({}): {}", status, err_msg))
        }
    }
}
