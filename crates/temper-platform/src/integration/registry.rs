//! Integration registry: maps trigger events to integration configs.

use std::collections::BTreeMap;

use super::types::IntegrationConfig;

/// Maps trigger event names to integration configurations.
///
/// Built once from the tenant's specs at registration time.
#[derive(Debug, Clone, Default)]
pub struct IntegrationRegistry {
    /// Maps event name to list of integrations triggered by that event.
    by_trigger: BTreeMap<String, Vec<IntegrationConfig>>,
}

impl IntegrationRegistry {
    /// Build a registry from a list of integration configs.
    pub fn from_configs(configs: Vec<IntegrationConfig>) -> Self {
        let mut by_trigger = BTreeMap::new();
        for config in configs {
            by_trigger
                .entry(config.trigger.clone())
                .or_insert_with(Vec::new)
                .push(config);
        }
        Self { by_trigger }
    }

    /// Look up integrations triggered by the given event name.
    pub fn lookup(&self, event_name: &str) -> &[IntegrationConfig] {
        self.by_trigger
            .get(event_name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Returns true if no integrations are registered.
    pub fn is_empty(&self) -> bool {
        self.by_trigger.is_empty()
    }

    /// Total number of registered integration configs.
    pub fn len(&self) -> usize {
        self.by_trigger.values().map(|v| v.len()).sum()
    }

    /// All registered trigger event names.
    pub fn triggers(&self) -> Vec<&str> {
        self.by_trigger.keys().map(|k| k.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integration::types::{RetryPolicy, WebhookConfig};

    fn test_config(name: &str, trigger: &str) -> IntegrationConfig {
        IntegrationConfig {
            name: name.to_string(),
            trigger: trigger.to_string(),
            webhook: WebhookConfig {
                url: format!("https://example.com/{name}"),
                method: "POST".to_string(),
                headers: Default::default(),
                timeout_ms: 5000,
            },
            retry: RetryPolicy::default(),
        }
    }

    #[test]
    fn lookup_returns_matching_configs() {
        let registry = IntegrationRegistry::from_configs(vec![
            test_config("fulfillment", "SubmitOrder"),
            test_config("payment", "ConfirmOrder"),
            test_config("analytics", "SubmitOrder"),
        ]);

        let submit_integrations = registry.lookup("SubmitOrder");
        assert_eq!(submit_integrations.len(), 2);
        assert_eq!(submit_integrations[0].name, "fulfillment");
        assert_eq!(submit_integrations[1].name, "analytics");

        let confirm_integrations = registry.lookup("ConfirmOrder");
        assert_eq!(confirm_integrations.len(), 1);
    }

    #[test]
    fn lookup_unknown_returns_empty() {
        let registry = IntegrationRegistry::from_configs(vec![test_config(
            "fulfillment",
            "SubmitOrder",
        )]);
        assert!(registry.lookup("Unknown").is_empty());
    }

    #[test]
    fn empty_registry() {
        let registry = IntegrationRegistry::from_configs(vec![]);
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }
}
