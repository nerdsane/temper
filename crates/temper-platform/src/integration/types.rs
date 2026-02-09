//! Core types for the integration engine.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration for a single integration endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationConfig {
    /// Integration name (matches the IOA `[[integration]]` name).
    pub name: String,
    /// The event that triggers this integration.
    pub trigger: String,
    /// Webhook configuration.
    pub webhook: WebhookConfig,
    /// Retry policy for failed deliveries.
    #[serde(default)]
    pub retry: RetryPolicy,
}

/// Webhook endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Target URL.
    pub url: String,
    /// HTTP method (default: POST).
    #[serde(default = "default_post")]
    pub method: String,
    /// Additional headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Request timeout in milliseconds (default: 5000).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_post() -> String {
    "POST".to_string()
}

fn default_timeout_ms() -> u64 {
    5000
}

impl WebhookConfig {
    /// Get the timeout as a [`Duration`].
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

/// Retry policy for failed integration deliveries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Base backoff duration in milliseconds.
    #[serde(default = "default_backoff_ms")]
    pub backoff_base_ms: u64,
}

fn default_max_retries() -> u32 {
    3
}

fn default_backoff_ms() -> u64 {
    1000
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            backoff_base_ms: default_backoff_ms(),
        }
    }
}

impl RetryPolicy {
    /// Compute backoff duration for the given attempt number.
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let ms = self
            .backoff_base_ms
            .saturating_mul(2u64.saturating_pow(attempt));
        // Cap at 5 minutes.
        Duration::from_millis(ms.min(300_000))
    }
}

/// An event to be dispatched to integration endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationEvent {
    /// Tenant that owns this entity.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// Entity instance ID.
    pub entity_id: String,
    /// The event/action name that triggered the integration.
    pub event_name: String,
    /// Status before the transition.
    pub from_status: String,
    /// Status after the transition.
    pub to_status: String,
    /// Action parameters.
    pub params: Value,
    /// When the event occurred.
    pub timestamp: DateTime<Utc>,
}

/// Status of an integration dispatch attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IntegrationStatus {
    /// Dispatch succeeded.
    Success,
    /// Dispatch failed permanently (after all retries exhausted).
    Failed(String),
    /// Dispatch is being retried.
    Retrying {
        /// Current retry attempt number.
        attempt: u32,
    },
}

/// Result of dispatching an integration event.
#[derive(Debug, Clone)]
pub struct IntegrationResult {
    /// Integration name.
    pub name: String,
    /// Dispatch status.
    pub status: IntegrationStatus,
    /// Total time taken.
    pub duration: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_defaults() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.backoff_base_ms, 1000);
    }

    #[test]
    fn backoff_exponential() {
        let policy = RetryPolicy {
            max_retries: 5,
            backoff_base_ms: 1000,
        };
        assert_eq!(policy.backoff_for(0), Duration::from_millis(1000));
        assert_eq!(policy.backoff_for(1), Duration::from_millis(2000));
        assert_eq!(policy.backoff_for(2), Duration::from_millis(4000));
        assert_eq!(policy.backoff_for(3), Duration::from_millis(8000));
    }

    #[test]
    fn backoff_caps_at_five_minutes() {
        let policy = RetryPolicy {
            max_retries: 10,
            backoff_base_ms: 60_000,
        };
        // 60_000 * 2^5 = 1_920_000 > 300_000, should cap.
        assert_eq!(policy.backoff_for(5), Duration::from_millis(300_000));
    }

    #[test]
    fn webhook_config_timeout_conversion() {
        let cfg = WebhookConfig {
            url: "https://example.com".to_string(),
            method: "POST".to_string(),
            headers: Default::default(),
            timeout_ms: 3000,
        };
        assert_eq!(cfg.timeout(), Duration::from_secs(3));
    }

    #[test]
    fn integration_event_serialization_roundtrip() {
        let event = IntegrationEvent {
            tenant: "alpha".to_string(),
            entity_type: "Order".to_string(),
            entity_id: "order-123".to_string(),
            event_name: "SubmitOrder".to_string(),
            from_status: "Draft".to_string(),
            to_status: "Submitted".to_string(),
            params: serde_json::json!({"ShippingAddressId": "addr-1"}),
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&event).expect("should serialize");
        let deserialized: IntegrationEvent =
            serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(deserialized.tenant, "alpha");
        assert_eq!(deserialized.event_name, "SubmitOrder");
    }

    #[test]
    fn integration_config_serde_with_defaults() {
        let json = r#"{
            "name": "test_hook",
            "trigger": "SubmitOrder",
            "webhook": {
                "url": "https://example.com/hook"
            }
        }"#;
        let config: IntegrationConfig =
            serde_json::from_str(json).expect("should deserialize with defaults");
        assert_eq!(config.name, "test_hook");
        assert_eq!(config.webhook.method, "POST");
        assert_eq!(config.webhook.timeout_ms, 5000);
        assert_eq!(config.retry.max_retries, 3);
        assert_eq!(config.retry.backoff_base_ms, 1000);
    }
}
