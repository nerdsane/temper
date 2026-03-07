//! Types for WASM module invocation.

use serde::{Deserialize, Serialize};

/// Context passed to a WASM module invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmInvocationContext {
    /// Tenant that owns the entity.
    pub tenant: String,
    /// Entity type (e.g. "Order").
    pub entity_type: String,
    /// Entity instance ID.
    pub entity_id: String,
    /// The action that triggered this integration.
    pub trigger_action: String,
    /// Parameters from the triggering action.
    pub trigger_params: serde_json::Value,
    /// Current entity state snapshot (fields JSON).
    pub entity_state: serde_json::Value,
    /// Agent that triggered this invocation (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Session that triggered this invocation (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Configuration from the [[integration]] section (url, method, headers, etc.).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub integration_config: std::collections::BTreeMap<String, String>,
}

/// Result returned from a WASM module invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmInvocationResult {
    /// The callback action to dispatch (e.g. "ChargeSucceeded").
    pub callback_action: String,
    /// Parameters for the callback action.
    pub callback_params: serde_json::Value,
    /// Whether the integration succeeded.
    pub success: bool,
    /// Error message if the integration failed.
    pub error: Option<String>,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
}

/// Resource limits for WASM module execution (TigerStyle budgets).
#[derive(Debug, Clone)]
pub struct WasmResourceLimits {
    /// Maximum fuel (instruction budget). Default: 1 billion.
    pub max_fuel: u64,
    /// Maximum memory in bytes. Default: 16 MB.
    pub max_memory: usize,
    /// Maximum execution duration. Default: 30 seconds.
    pub max_duration: std::time::Duration,
    /// Maximum HTTP response body size. Default: 1 MB.
    pub max_response_bytes: usize,
}

impl Default for WasmResourceLimits {
    fn default() -> Self {
        Self {
            max_fuel: 1_000_000_000,
            max_memory: 16 * 1024 * 1024,
            max_duration: std::time::Duration::from_secs(30),
            max_response_bytes: 1024 * 1024,
        }
    }
}

/// Maximum WASM module size (TigerStyle budget). 10 MB.
pub const MAX_MODULE_SIZE: usize = 10 * 1024 * 1024;

/// Authorization context for WASM host function calls.
///
/// Carries identity and scope information so the authorization gate
/// can make fine-grained decisions about HTTP calls and secret access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmAuthzContext {
    /// Tenant that owns the entity.
    pub tenant: String,
    /// WASM module name (used as Cedar principal ID).
    pub module_name: String,
    /// Agent that triggered this invocation (if known).
    pub agent_id: Option<String>,
    /// Session that triggered this invocation (if known).
    pub session_id: Option<String>,
    /// Entity type being operated on.
    pub entity_type: String,
    /// The action that triggered this WASM invocation.
    pub trigger_action: String,
}

#[cfg(any(test, feature = "test-helpers"))]
impl WasmAuthzContext {
    /// Build a test fixture context.
    pub fn test_fixture() -> Self {
        Self {
            tenant: "test-tenant".into(),
            module_name: "stripe_charge".into(),
            agent_id: Some("agent-1".into()),
            session_id: None,
            entity_type: "Order".into(),
            trigger_action: "submitOrder".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_context_serde_roundtrip() {
        let ctx = WasmInvocationContext {
            tenant: "t1".into(),
            entity_type: "Order".into(),
            entity_id: "ORD-1".into(),
            trigger_action: "Submit".into(),
            trigger_params: serde_json::json!({"amount": 100}),
            entity_state: serde_json::json!({"status": "Draft"}),
            agent_id: Some("agent-1".into()),
            session_id: None,
            integration_config: std::collections::BTreeMap::new(),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: WasmInvocationContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tenant, "t1");
        assert_eq!(back.entity_type, "Order");
        assert_eq!(back.agent_id, Some("agent-1".into()));
        assert!(back.session_id.is_none());
    }

    #[test]
    fn invocation_context_skips_empty_optional_fields() {
        let ctx = WasmInvocationContext {
            tenant: "t".into(),
            entity_type: "E".into(),
            entity_id: "1".into(),
            trigger_action: "A".into(),
            trigger_params: serde_json::Value::Null,
            entity_state: serde_json::Value::Null,
            agent_id: None,
            session_id: None,
            integration_config: std::collections::BTreeMap::new(),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(!json.contains("agent_id"));
        assert!(!json.contains("session_id"));
        assert!(!json.contains("integration_config"));
    }

    #[test]
    fn invocation_result_serde_roundtrip() {
        let result = WasmInvocationResult {
            callback_action: "PaymentConfirmed".into(),
            callback_params: serde_json::json!({"ref": "tx-123"}),
            success: true,
            error: None,
            duration_ms: 250,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: WasmInvocationResult = serde_json::from_str(&json).unwrap();
        assert!(back.success);
        assert_eq!(back.callback_action, "PaymentConfirmed");
        assert_eq!(back.duration_ms, 250);
    }

    #[test]
    fn resource_limits_defaults() {
        let limits = WasmResourceLimits::default();
        assert_eq!(limits.max_fuel, 1_000_000_000);
        assert_eq!(limits.max_memory, 16 * 1024 * 1024);
        assert_eq!(limits.max_duration, std::time::Duration::from_secs(30));
        assert_eq!(limits.max_response_bytes, 1024 * 1024);
    }

    #[test]
    fn max_module_size_is_10mb() {
        assert_eq!(MAX_MODULE_SIZE, 10 * 1024 * 1024);
    }

    #[test]
    fn authz_context_test_fixture() {
        let ctx = WasmAuthzContext::test_fixture();
        assert_eq!(ctx.tenant, "test-tenant");
        assert_eq!(ctx.module_name, "stripe_charge");
        assert_eq!(ctx.agent_id, Some("agent-1".into()));
    }
}
