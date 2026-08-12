//! Native integration adapters for `type = "adapter"` execution.
//!
//! ADR-0160 / ARN-228: the kernel keeps only a generic HTTP adapter behind a
//! fail-closed egress gate. App-specific agent CLIs (Claude Code, Codex,
//! OpenClaw) are **not** registered here — they execute in capability-scoped
//! TemperPaw workers (follow-up).

mod egress;
mod http_webhook;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

pub use egress::{
    ADAPTER_HTTP_TIMEOUT_SECS, ADAPTER_MAX_RESPONSE_BYTES, is_blocked_ip, validate_adapter_http_url,
};
pub use http_webhook::HttpWebhookAdapter;

/// Agent identity context provided to adapter executions.
///
/// Platform credentials (`agent_api_key`) are intentionally never populated
/// for in-kernel adapters (ADR-0160).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AdapterAgentContext {
    /// Calling principal ID.
    pub agent_id: Option<String>,
    /// Calling session identifier.
    pub session_id: Option<String>,
    /// Calling agent type classification.
    pub agent_type: Option<String>,
    /// Deprecated: always `None` for in-kernel adapters (ARN-228).
    /// Kept on the struct so serde of historical test fixtures stays stable.
    #[serde(skip)]
    pub agent_api_key: Option<String>,
}

/// Full adapter invocation context built from dispatch state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdapterContext {
    /// Tenant identifier.
    pub tenant: String,
    /// Entity type being dispatched.
    pub entity_type: String,
    /// Entity ID being dispatched.
    pub entity_id: String,
    /// Trigger action name.
    pub trigger_action: String,
    /// Trigger action parameters.
    pub trigger_params: serde_json::Value,
    /// Serialized current entity state.
    pub entity_state: serde_json::Value,
    /// Integration config with secret templates resolved (least privilege).
    pub integration_config: BTreeMap<String, String>,
    /// Agent identity context (no ambient platform credential).
    pub agent_ctx: AdapterAgentContext,
    /// Always empty for in-kernel adapters (ADR-0160). Secret values appear only
    /// where `{secret:KEY}` templates expanded into `integration_config`.
    pub secrets: BTreeMap<String, String>,
}

impl AdapterContext {
    /// Retrieve a secret value by key.
    ///
    /// Always returns `None` for production in-kernel adapters — the full
    /// tenant secret map is never attached (ARN-228).
    pub fn get_secret(&self, key: &str) -> Option<String> {
        self.secrets.get(key).cloned()
    }
}

/// Adapter invocation result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdapterResult {
    /// Optional callback action suggested by the adapter implementation.
    pub callback_action: Option<String>,
    /// Callback params produced by the adapter.
    pub callback_params: serde_json::Value,
    /// Whether adapter execution succeeded.
    pub success: bool,
    /// Optional failure description when `success` is false.
    pub error: Option<String>,
    /// End-to-end adapter runtime duration.
    pub duration_ms: u64,
}

impl AdapterResult {
    /// Build a successful adapter result.
    pub fn success(callback_params: serde_json::Value, duration_ms: u64) -> Self {
        Self {
            callback_action: None,
            callback_params,
            success: true,
            error: None,
            duration_ms,
        }
    }

    /// Build a failed adapter result.
    pub fn failure(error: String, duration_ms: u64) -> Self {
        Self {
            callback_action: None,
            callback_params: serde_json::json!({}),
            success: false,
            error: Some(error),
            duration_ms,
        }
    }
}

/// Typed adapter execution errors.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// Adapter invocation could not be started.
    #[error("adapter invocation failed: {0}")]
    Invocation(String),
    /// Adapter execution failed with runtime error.
    #[error("adapter execution failed: {0}")]
    Execution(String),
    /// Adapter output could not be parsed.
    #[error("adapter output parse failed: {0}")]
    Parse(String),
}

/// Trait implemented by all native adapter integrations.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    /// Stable adapter type key used for registry lookup.
    fn adapter_type(&self) -> &str;

    /// Execute this adapter with the provided invocation context.
    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError>;
}

/// Registry of available adapter implementations keyed by adapter type.
#[derive(Clone, Default)]
pub struct AdapterRegistry {
    /// Registered adapter implementations.
    adapters: BTreeMap<String, Arc<dyn AgentAdapter>>,
}

impl AdapterRegistry {
    /// Create an empty adapter registry.
    pub fn new() -> Self {
        Self {
            adapters: BTreeMap::new(),
        }
    }

    /// Create a registry with built-in **kernel-safe** adapters only.
    ///
    /// ADR-0160: no Claude Code / Codex / OpenClaw process spawners.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(HttpWebhookAdapter));
        registry
    }

    /// Register an adapter implementation.
    pub fn register(&mut self, adapter: Arc<dyn AgentAdapter>) {
        self.adapters
            .insert(adapter.adapter_type().to_string(), adapter);
    }

    /// Resolve an adapter by type key.
    pub fn get(&self, adapter_type: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.adapters.get(adapter_type).cloned()
    }

    /// Return all registered adapter type keys in deterministic order.
    pub fn adapter_types(&self) -> Vec<String> {
        self.adapters.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::AdapterRegistry;

    #[test]
    fn builtins_are_http_only() {
        let registry = AdapterRegistry::with_builtins();
        let adapter_types = registry.adapter_types();
        assert_eq!(adapter_types, vec!["http".to_string()]);
        assert!(registry.get("claude_code").is_none());
        assert!(registry.get("codex").is_none());
        assert!(registry.get("openclaw").is_none());
        assert!(registry.get("http").is_some());
        assert!(registry.get("missing").is_none());
    }
}
