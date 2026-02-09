//! Webhook dispatcher: sends HTTP requests to integration endpoints.

use std::time::Instant;

use tracing::{info, warn};

use super::types::{IntegrationConfig, IntegrationEvent, IntegrationResult, IntegrationStatus};

/// Dispatches webhook HTTP requests to integration endpoints.
pub struct WebhookDispatcher {
    /// HTTP client (shared across dispatches).
    client: reqwest::Client,
}

impl WebhookDispatcher {
    /// Create a new dispatcher.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Dispatch a single integration event, respecting retry policy.
    pub async fn dispatch(
        &self,
        config: &IntegrationConfig,
        event: &IntegrationEvent,
    ) -> IntegrationResult {
        let start = Instant::now();
        let mut last_error = String::new();

        for attempt in 0..=config.retry.max_retries {
            if attempt > 0 {
                let backoff = config.retry.backoff_for(attempt - 1);
                tokio::time::sleep(backoff).await;
            }

            match self.dispatch_once(config, event).await {
                Ok(()) => {
                    info!(
                        integration = %config.name,
                        trigger = %config.trigger,
                        entity = %event.entity_id,
                        attempt,
                        "integration dispatch succeeded"
                    );
                    return IntegrationResult {
                        name: config.name.clone(),
                        status: IntegrationStatus::Success,
                        duration: start.elapsed(),
                    };
                }
                Err(e) => {
                    last_error = e.to_string();
                    warn!(
                        integration = %config.name,
                        trigger = %config.trigger,
                        entity = %event.entity_id,
                        attempt,
                        error = %last_error,
                        "integration dispatch failed, will retry"
                    );
                }
            }
        }

        IntegrationResult {
            name: config.name.clone(),
            status: IntegrationStatus::Failed(last_error),
            duration: start.elapsed(),
        }
    }

    /// Send a single HTTP request (no retry).
    async fn dispatch_once(
        &self,
        config: &IntegrationConfig,
        event: &IntegrationEvent,
    ) -> Result<(), reqwest::Error> {
        let timeout = config.webhook.timeout();

        let mut req = match config.webhook.method.to_uppercase().as_str() {
            "PUT" => self.client.put(&config.webhook.url),
            _ => self.client.post(&config.webhook.url),
        };

        req = req.timeout(timeout).json(event);

        for (key, value) in &config.webhook.headers {
            req = req.header(key.as_str(), value.as_str());
        }

        let response = req.send().await?;
        // Treat any 2xx as success; error on 4xx/5xx.
        response.error_for_status()?;
        Ok(())
    }
}

impl Default for WebhookDispatcher {
    fn default() -> Self {
        Self::new()
    }
}
