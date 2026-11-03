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
    /// WASM module name being invoked, when known by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasm_module: Option<String>,
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
    /// Configuration from the triggering `[[action.triggers]]` block (url, method, headers, etc.).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub integration_config: std::collections::BTreeMap<String, String>,
    /// W3C trace ID for cross-request trace correlation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trace_id: String,
    /// Logical workflow root entity type for cross-request observability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_root_entity_type: Option<String>,
    /// Logical workflow root entity id for cross-request observability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_root_entity_id: Option<String>,
    /// Stable workflow run id for grouping async work in APM/logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    /// When the invocation was triggered by an HttpEndpoint dispatch
    /// (ADR-0069 Phase 2), carries the HTTP-specific context so the
    /// guest can unpack method, path params, headers, and the stream
    /// handle IDs needed to read the request body + write the
    /// response. `None` for entity-action-triggered invocations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_request: Option<HttpDispatchContext>,
    /// Who the move that set this integration off was made as: the
    /// principal Cedar saw for that move. A timer's move is its setter's,
    /// and a reaction's is the move it reacts to's. `None` when the move was
    /// made by the kernel itself (a system or service move) or anonymously.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_principal: Option<TriggerPrincipal>,
}

/// The principal a triggering move was made as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerPrincipal {
    /// The principal's id: the person, or the agent instance.
    pub id: String,
    /// `Customer` (a person), `Agent` or `Admin`.
    pub kind: String,
    /// For an agent, the person it acts for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acting_for: Option<String>,
    /// For an agent, its type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

/// HTTP-specific invocation context, attached to `WasmInvocationContext`
/// when the invocation is driven by an `HttpEndpoint` dispatch.
///
/// Guests read this to:
///   * dispatch on `method` + `path`;
///   * extract named path parameters from `params`;
///   * access headers;
///   * stream the request body from `request_body_handle`;
///   * write the response body to `response_body_handle`;
///   * fire the response head via `host_http_stream_send_response_head`
///     keyed on `response_body_handle`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpDispatchContext {
    /// Uppercase HTTP method (GET, POST, PUT, PATCH, DELETE).
    pub method: String,
    /// Full request path (including leading slash). The router has
    /// already matched it against the HttpEndpoint's `path_prefix`;
    /// this is the verbatim value as received by axum so guests can
    /// inspect the tail after the prefix.
    pub path: String,
    /// Path parameters extracted from the prefix's `{name}` segments.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub params: std::collections::BTreeMap<String, String>,
    /// Request headers, lowercase keys, string values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Resolved principal (if the route had `requires_auth = true`).
    /// Guests can use this for Cedar lookups; kernel has already
    /// validated the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// Stream handle ID the guest reads request body chunks from.
    /// Dispatched into by the kernel from the axum body pump task.
    pub request_body_handle: u32,
    /// Stream handle ID the guest writes response body chunks to.
    /// Dispatched out by the kernel into the axum response stream.
    /// Same handle is passed to `host_http_stream_send_response_head`
    /// to deliver the HTTP response head.
    pub response_body_handle: u32,
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
    /// The module answered empty (`"empty": true`): it has nothing to
    /// report, and the kernel moves nothing, whatever the trigger's
    /// `on_success` names. `"action": ""` is not empty: modules have long
    /// written it to mean "send `on_success`".
    #[serde(default)]
    pub empty_answer: bool,
}

/// Resource limits for WASM module execution (TigerStyle budgets).
#[derive(Debug, Clone)]
pub struct WasmResourceLimits {
    /// Maximum fuel (instruction budget). Default: 1 billion.
    pub max_fuel: u64,
    /// Maximum memory in bytes. Default: 64 MB.
    pub max_memory: usize,
    /// Maximum execution duration. Default: 120 seconds.
    ///
    /// Raised from 30s in ADR-0045 to cover HTTP-fronted integrations under load.
    pub max_duration: std::time::Duration,
    /// Maximum HTTP response body size. Default: 1 MB.
    pub max_response_bytes: usize,
}

impl Default for WasmResourceLimits {
    fn default() -> Self {
        Self {
            max_fuel: 1_000_000_000,
            max_memory: 64 * 1024 * 1024,
            max_duration: std::time::Duration::from_secs(120),
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
