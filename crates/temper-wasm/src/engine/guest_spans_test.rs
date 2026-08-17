use std::collections::BTreeMap;

use serde_json::Value;

use crate::types::WasmInvocationContext;

use super::*;

fn context() -> WasmInvocationContext {
    WasmInvocationContext {
        tenant: "test".to_string(),
        entity_type: "Session".to_string(),
        entity_id: "s-1".to_string(),
        trigger_action: "Run".to_string(),
        wasm_module: Some("test_guest".to_string()),
        trigger_params: Value::Null,
        entity_state: Value::Null,
        agent_id: Some("agent-1".to_string()),
        session_id: Some("session-1".to_string()),
        integration_config: BTreeMap::new(),
        trace_id: "00000000000000000000000000000001".to_string(),
        workflow_root_entity_type: Some("Session".to_string()),
        workflow_root_entity_id: Some("s-1".to_string()),
        workflow_run_id: Some("Session:s-1".to_string()),
        http_request: None,
    }
}

#[test]
fn rejects_malformed_and_empty_start_payloads() {
    let mut registry = GuestSpanRegistry::new(context());
    assert!(registry.start_span("{").is_err());
    assert!(registry.start_span(r#"{"name":""}"#).is_err());
}

#[test]
fn enforces_nested_lifo_end_order() {
    let mut registry = GuestSpanRegistry::new(context());
    let root = registry
        .start_span(r#"{"name":"guest.root"}"#)
        .expect("root span should start");
    let child = registry
        .start_span(r#"{"name":"guest.child"}"#)
        .expect("child span should start");

    assert!(registry.end_span(root, r#"{"status":"ok"}"#).is_err());
    registry
        .end_span(child, r#"{"status":"ok"}"#)
        .expect("child can end first");
    registry
        .end_span(root, r#"{"status":"ok"}"#)
        .expect("root can end after child");
}

#[test]
fn rejects_unknown_span_ids() {
    let mut registry = GuestSpanRegistry::new(context());
    assert!(registry.add_span_event(99, r#"{"name":"event"}"#).is_err());
    assert!(
        registry
            .set_span_attributes(99, r#"{"attributes":{}}"#)
            .is_err()
    );
    assert!(registry.end_span(99, r#"{"status":"ok"}"#).is_err());
}

#[test]
fn enforces_per_invocation_span_limit() {
    let mut registry = GuestSpanRegistry::with_max_spans(context(), 1);
    registry
        .start_span(r#"{"name":"guest.root"}"#)
        .expect("first span should start");
    assert!(registry.start_span(r#"{"name":"guest.child"}"#).is_err());
}

#[test]
fn protects_reserved_observability_fields() {
    assert!(!guest_span_attribute_allowed("trace_id"));
    assert!(!guest_span_attribute_allowed("dd.span_id"));
    assert!(!guest_span_attribute_allowed("_otel.parent_trace_id"));
    assert!(guest_span_attribute_allowed("dd_llmobs_enabled"));
    assert!(guest_span_attribute_allowed("tool.name"));
    assert!(guest_span_attribute_allowed("gen_ai.operation.name"));
}

#[test]
fn manual_span_snapshot_includes_invocation_correlation_attributes() {
    let mut attributes = BTreeMap::new();
    attributes.insert("tool.name".to_string(), Value::String("python".to_string()));
    let snapshot = manual_span_attributes(&context(), 7, &attributes, false);

    assert_eq!(
        snapshot.get("tenant"),
        Some(&Value::String("test".to_string()))
    );
    assert_eq!(
        snapshot.get("entity_id"),
        Some(&Value::String("s-1".to_string()))
    );
    assert_eq!(
        snapshot.get("wasm_module"),
        Some(&Value::String("test_guest".to_string()))
    );
    assert_eq!(
        snapshot.get("workflow_run_id"),
        Some(&Value::String("Session:s-1".to_string()))
    );
    assert_eq!(
        snapshot.get("observability_event"),
        Some(&Value::String("wasm_guest.span".to_string()))
    );
    assert_eq!(snapshot.get("wasm_guest_span_id"), Some(&Value::from(7)));
    assert_eq!(
        snapshot.get("tool.name"),
        Some(&Value::String("python".to_string()))
    );
}

#[test]
fn cleanup_closes_unended_spans() {
    let mut registry = GuestSpanRegistry::new(context());
    registry
        .start_span(r#"{"name":"guest.root"}"#)
        .expect("span should start");
    assert!(registry.enter_active().is_some());
    registry.cleanup_unclosed();
    assert!(registry.enter_active().is_none());
}

/// ARN-243 / ADR-0166. The guest manual-span API lets an untrusted module name
/// its own span attributes, including the canonical `gen_ai.*` keys that LLM
/// Observability reads. For a tenant that has not opted into content export, a
/// module holding a prompt and a completion must not be able to publish them
/// simply by calling `host_start_span` with those names.
#[test]
fn guest_span_attributes_drop_llm_content_for_non_opted_in_tenant() {
    let mut attributes = BTreeMap::new();
    attributes.insert(
        "gen_ai.input.messages".to_string(),
        Value::String("SECRET PROMPT".to_string()),
    );
    attributes.insert(
        "gen_ai.completion".to_string(),
        Value::String("SECRET COMPLETION".to_string()),
    );
    attributes.insert(
        "gen_ai.request.model".to_string(),
        Value::String("claude-opus-4-8".to_string()),
    );
    // A name inside the namespace that no denylist enumerates.
    attributes.insert(
        "gen_ai.response.text".to_string(),
        Value::String("SECRET COMPLETION".to_string()),
    );
    // The module's own application telemetry, which must keep working.
    attributes.insert("order.id".to_string(), Value::String("A-17".to_string()));

    let redacted = manual_span_attributes(&context(), 1, &attributes, false);

    assert_eq!(
        redacted.get("gen_ai.input.messages"),
        None,
        "prompt must not export"
    );
    assert_eq!(
        redacted.get("gen_ai.completion"),
        None,
        "completion must not export"
    );
    assert_eq!(
        redacted.get("gen_ai.response.text"),
        None,
        "an unrecognized gen_ai.* key must not export just because it is unlisted"
    );
    assert_eq!(
        redacted.get("gen_ai.request.model"),
        Some(&Value::String("claude-opus-4-8".to_string())),
        "recognized metadata must survive"
    );
    assert_eq!(
        redacted.get("order.id"),
        Some(&Value::String("A-17".to_string())),
        "non-LLM guest telemetry must keep working"
    );

    // The opted-in tenant is unaffected.
    let exported = manual_span_attributes(&context(), 1, &attributes, true);
    assert_eq!(
        exported.get("gen_ai.completion"),
        Some(&Value::String("SECRET COMPLETION".to_string())),
        "an opted-in tenant still exports content"
    );
}

/// A key name cannot make an untrusted value into metadata: without a bound, a
/// module hides the whole prompt inside `gen_ai.request.model` and the allowlist
/// waves it through.
#[test]
fn guest_span_metadata_values_are_bounded_for_non_opted_in_tenant() {
    let prompt = "P".repeat(4096);
    let mut attributes = BTreeMap::new();
    attributes.insert(
        "gen_ai.request.model".to_string(),
        Value::String(prompt.clone()),
    );

    let redacted = manual_span_attributes(&context(), 1, &attributes, false);
    let Some(Value::String(value)) = redacted.get("gen_ai.request.model") else {
        panic!("metadata key should survive, bounded");
    };
    assert!(
        value.len() <= 256,
        "metadata value must be clamped, got {} bytes",
        value.len()
    );

    let exported = manual_span_attributes(&context(), 1, &attributes, true);
    assert_eq!(
        exported.get("gen_ai.request.model"),
        Some(&Value::String(prompt)),
        "an opted-in tenant is not clamped"
    );
}
