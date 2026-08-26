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
    /// Configuration from the [[integration]] section (url, method, headers, etc.).
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
    /// Guest-declared typed application facts, when this is a typed failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typed_failure: Option<temper_failure::GuestFailureDeclarationV1>,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
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

/// Maximum serialized terminal result accepted from a WASM guest (1 MiB).
pub const MAX_WASM_RESULT_BYTES_V1: usize = 1024 * 1024;

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
            wasm_module: Some("order_submitter".into()),
            trigger_params: serde_json::json!({"amount": 100}),
            entity_state: serde_json::json!({"status": "Draft"}),
            agent_id: Some("agent-1".into()),
            session_id: None,
            integration_config: std::collections::BTreeMap::new(),
            trace_id: String::new(),
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            http_request: None,
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: WasmInvocationContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tenant, "t1");
        assert_eq!(back.entity_type, "Order");
        assert_eq!(back.wasm_module, Some("order_submitter".into()));
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
            wasm_module: None,
            trigger_params: serde_json::Value::Null,
            entity_state: serde_json::Value::Null,
            agent_id: None,
            session_id: None,
            integration_config: std::collections::BTreeMap::new(),
            trace_id: String::new(),
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            http_request: None,
        };
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(!json.contains("agent_id"));
        assert!(!json.contains("session_id"));
        assert!(!json.contains("wasm_module"));
        assert!(!json.contains("integration_config"));
        assert!(!json.contains("workflow_root_entity_type"));
        assert!(!json.contains("workflow_root_entity_id"));
        assert!(!json.contains("workflow_run_id"));
        assert!(!json.contains("http_request"));
    }

    #[test]
    fn http_dispatch_context_serde_roundtrip() {
        let http = HttpDispatchContext {
            method: "POST".into(),
            path: "/repos/acme/widgets.git/git-upload-pack".into(),
            params: std::collections::BTreeMap::from([
                ("owner".to_string(), "acme".to_string()),
                ("repo".to_string(), "widgets".to_string()),
            ]),
            headers: vec![(
                "content-type".into(),
                "application/x-git-upload-pack-request".into(),
            )],
            principal_id: Some("gt-01".into()),
            request_body_handle: 1,
            response_body_handle: 2,
        };
        let json = serde_json::to_string(&http).unwrap();
        let back: HttpDispatchContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, "POST");
        assert_eq!(back.params.get("owner"), Some(&"acme".to_string()));
        assert_eq!(back.request_body_handle, 1);
        assert_eq!(back.response_body_handle, 2);
    }

    #[test]
    fn invocation_context_with_http_request_serializes() {
        let ctx = WasmInvocationContext {
            tenant: "temper-git".into(),
            entity_type: "HttpEndpoint".into(),
            entity_id: "he-1".into(),
            trigger_action: "HandleHttp".into(),
            wasm_module: Some("git_http_endpoint".into()),
            trigger_params: serde_json::Value::Null,
            entity_state: serde_json::Value::Null,
            agent_id: None,
            session_id: None,
            integration_config: std::collections::BTreeMap::new(),
            trace_id: String::new(),
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            http_request: Some(HttpDispatchContext {
                method: "GET".into(),
                path: "/repos/acme/widgets.git/info/refs".into(),
                params: std::collections::BTreeMap::new(),
                headers: vec![],
                principal_id: None,
                request_body_handle: 10,
                response_body_handle: 11,
            }),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(json.contains("http_request"));
        assert!(json.contains("request_body_handle"));
        let back: WasmInvocationContext = serde_json::from_str(&json).unwrap();
        assert!(back.http_request.is_some());
        assert_eq!(back.http_request.unwrap().method, "GET");
    }

    #[test]
    fn invocation_result_serde_roundtrip() {
        let result = WasmInvocationResult {
            callback_action: "PaymentConfirmed".into(),
            callback_params: serde_json::json!({"ref": "tx-123"}),
            success: true,
            error: None,
            typed_failure: None,
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
        assert_eq!(limits.max_memory, 64 * 1024 * 1024);
        // ADR-0045: raised from 30s to cover HTTP-fronted integrations under load.
        assert_eq!(limits.max_duration, std::time::Duration::from_secs(120));
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
