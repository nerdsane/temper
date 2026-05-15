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
    assert!(guest_span_attribute_allowed("tool.name"));
    assert!(guest_span_attribute_allowed("gen_ai.operation.name"));
}

#[test]
fn manual_span_snapshot_includes_invocation_correlation_attributes() {
    let mut attributes = BTreeMap::new();
    attributes.insert("tool.name".to_string(), Value::String("python".to_string()));
    let snapshot = manual_span_attributes(&context(), 7, &attributes);

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
