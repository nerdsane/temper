//! Native agent adapter integrations for `type = "adapter"` execution.
//!
//! Adapters run in platform Rust code (not WASM), enabling capabilities like
//! CLI process execution and WebSocket gateway sessions while preserving
//! IOA-declared integration intent.

mod claude_code;
mod codex;
mod http_webhook;
mod openclaw;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

pub use claude_code::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use http_webhook::HttpWebhookAdapter;
pub use openclaw::OpenClawAdapter;

/// Agent identity context provided to adapter executions.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AdapterAgentContext {
    /// Calling principal ID.
    pub agent_id: Option<String>,
    /// Calling session identifier.
    pub session_id: Option<String>,
    /// Calling agent type classification.
    pub agent_type: Option<String>,
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
    /// Integration config with secret templates resolved.
    pub integration_config: BTreeMap<String, String>,
    /// Agent identity context.
    pub agent_ctx: AdapterAgentContext,
    /// Per-tenant secrets snapshot for adapter use.
    pub secrets: BTreeMap<String, String>,
}

impl AdapterContext {
    /// Retrieve a secret value by key from the invocation snapshot.
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

    /// Create a registry with built-in adapter implementations registered.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(ClaudeCodeAdapter));
        registry.register(Arc::new(CodexAdapter));
        registry.register(Arc::new(OpenClawAdapter));
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
    fn builtins_are_registered() {
        let registry = AdapterRegistry::with_builtins();
        let adapter_types = registry.adapter_types();
        assert!(adapter_types.contains(&"claude_code".to_string()));
        assert!(adapter_types.contains(&"codex".to_string()));
        assert!(adapter_types.contains(&"openclaw".to_string()));
        assert!(adapter_types.contains(&"http".to_string()));
    }

    #[test]
    fn lookup_returns_registered_adapter() {
        let registry = AdapterRegistry::with_builtins();
        assert!(registry.get("claude_code").is_some());
        assert!(registry.get("codex").is_some());
        assert!(registry.get("openclaw").is_some());
        assert!(registry.get("http").is_some());
        assert!(registry.get("missing").is_none());
    }
}
