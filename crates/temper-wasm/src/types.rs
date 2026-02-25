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
