use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::SystemTime;

use opentelemetry::KeyValue;
use opentelemetry::trace::{
    Event, Span as OtelSpan, SpanId, SpanKind, Status, TraceContextExt as _, TraceId, Tracer,
};
use serde::Deserialize;
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::host_trait::{datadog_visible_span_hint_field, truncate_for_span_attr};
use crate::types::WasmInvocationContext;

const DEFAULT_MAX_GUEST_SPANS: usize = 256;

#[derive(Debug, Deserialize)]
struct GuestSpanStartPayload {
    name: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct GuestSpanEventPayload {
    name: String,
    #[serde(default)]
    attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct GuestSpanAttributesPayload {
    #[serde(default)]
    attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct GuestSpanEndPayload {
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "error.type")]
    error_type: Option<String>,
    #[serde(default, rename = "error.message")]
    error_message: Option<String>,
    #[serde(default)]
    attributes: BTreeMap<String, Value>,
}

thread_local! {
    // `EnteredSpan` is intentionally !Send, while Wasmtime host state must be
    // Send. Guest WASM execution is synchronous on one blocking thread, so keep
    // the active enter guards thread-local and the registry itself portable.
    static ENTERED_GUEST_SPANS: RefCell<Vec<tracing::span::EnteredSpan>> =
        const { RefCell::new(Vec::new()) };
}

struct GuestSpanEntry {
    span: tracing::Span,
    name: String,
    kind: String,
    start_time: SystemTime,
    parent_context: Option<opentelemetry::Context>,
    trace_id: Option<TraceId>,
    span_id: Option<SpanId>,
    attributes: BTreeMap<String, Value>,
    events: Vec<GuestSpanManualEvent>,
}

struct GuestSpanManualEvent {
    name: String,
    timestamp: SystemTime,
    attributes: BTreeMap<String, Value>,
}

pub(crate) struct GuestSpanRegistry {
    context: WasmInvocationContext,
    manual_export: bool,
    next_id: i64,
    total_started: usize,
    max_spans: usize,
    spans: BTreeMap<i64, GuestSpanEntry>,
    stack: Vec<i64>,
}

impl GuestSpanRegistry {
    #[cfg(test)]
    pub(crate) fn new(context: WasmInvocationContext) -> Self {
        Self::with_manual_export(context, false)
    }

    pub(crate) fn for_invocation(context: WasmInvocationContext, needs_wasi: bool) -> Self {
        Self::with_manual_export(context, needs_wasi)
    }

    fn with_manual_export(context: WasmInvocationContext, manual_export: bool) -> Self {
        clear_thread_local_guest_spans();
        Self {
            context,
            manual_export,
            next_id: 1,
            total_started: 0,
            max_spans: DEFAULT_MAX_GUEST_SPANS,
            spans: BTreeMap::new(),
            stack: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_max_spans(context: WasmInvocationContext, max_spans: usize) -> Self {
        Self {
            max_spans,
            ..Self::new(context)
        }
    }

    pub(crate) fn start_span(&mut self, payload_json: &str) -> Result<i64, String> {
        let payload: GuestSpanStartPayload = serde_json::from_str(payload_json)
            .map_err(|e| format!("invalid guest span start payload: {e}"))?;
        let name = payload.name.trim();
        if name.is_empty() {
            return Err("guest span name must not be empty".to_string());
        }
        if self.spans.len() >= self.max_spans || self.total_started >= self.max_spans {
            return Err(format!(
                "guest span limit exceeded: max_spans={}",
                self.max_spans
            ));
        }

        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.total_started += 1;
        let kind = payload.kind.as_deref().unwrap_or("internal");
        let parent_context = manual_parent_context();
        let span = tracing::info_span!(
            "wasm.guest",
            otel.name = %name,
            otel.kind = %kind,
            tenant = %self.context.tenant,
            entity_type = %self.context.entity_type,
            entity_id = %self.context.entity_id,
            trigger_action = %self.context.trigger_action,
            wasm_module = self.context.wasm_module.as_deref().unwrap_or(""),
            agent_id = self.context.agent_id.as_deref().unwrap_or(""),
            session_id = self.context.session_id.as_deref().unwrap_or(""),
            workflow_root_entity_type = self.context.workflow_root_entity_type.as_deref().unwrap_or(""),
            workflow_root_entity_id = self.context.workflow_root_entity_id.as_deref().unwrap_or(""),
            workflow_run_id = self.context.workflow_run_id.as_deref().unwrap_or(""),
            observability_event = "wasm_guest.span",
            wasm_guest_span_id = id,
            action_name = tracing::field::Empty,
            managed_session_id = tracing::field::Empty,
            inner_session_id = tracing::field::Empty,
            parent_session_id = tracing::field::Empty,
            environment_id = tracing::field::Empty,
            workflow_step = tracing::field::Empty,
            tool.name = tracing::field::Empty,
            tool.call_id = tracing::field::Empty,
            gen_ai.operation.name = tracing::field::Empty,
            gen_ai.provider.name = tracing::field::Empty,
            gen_ai.request.model = tracing::field::Empty,
            error.type = tracing::field::Empty,
            error.message = tracing::field::Empty,
            exception.message = tracing::field::Empty,
        );
        update_span_name(&span, name);
        apply_attributes(&span, &payload.attributes);
        enter_thread_local_guest_span(&span);
        let (trace_id, span_id) = tracing_span_ids(&span);
        let attributes = manual_span_attributes(&self.context, id, &payload.attributes);

        self.spans.insert(
            id,
            GuestSpanEntry {
                span,
                name: name.to_string(),
                kind: kind.to_string(),
                start_time: SystemTime::now(),
                parent_context,
                trace_id,
                span_id,
                attributes,
                events: Vec::new(),
            },
        );
        self.stack.push(id);
        Ok(id)
    }

    pub(crate) fn add_span_event(
        &mut self,
        span_id: i64,
        payload_json: &str,
    ) -> Result<(), String> {
        let payload: GuestSpanEventPayload = serde_json::from_str(payload_json)
            .map_err(|e| format!("invalid guest span event payload: {e}"))?;
        let name = payload.name.trim();
        if name.is_empty() {
            return Err("guest span event name must not be empty".to_string());
        }
        let entry = self.span_mut(span_id)?;
        let attrs = payload
            .attributes
            .iter()
            .filter_map(|(key, value)| key_value_from_json(key, value))
            .collect::<Vec<_>>();
        entry
            .span
            .context()
            .span()
            .add_event(name.to_string(), attrs);
        entry.events.push(GuestSpanManualEvent {
            name: name.to_string(),
            timestamp: SystemTime::now(),
            attributes: allowed_attributes(&payload.attributes),
        });
        Ok(())
    }

    pub(crate) fn set_span_attributes(
        &mut self,
        span_id: i64,
        payload_json: &str,
    ) -> Result<(), String> {
        let payload: GuestSpanAttributesPayload = serde_json::from_str(payload_json)
            .map_err(|e| format!("invalid guest span attributes payload: {e}"))?;
        let entry = self.span_mut(span_id)?;
        apply_attributes(&entry.span, &payload.attributes);
        merge_allowed_attributes(&mut entry.attributes, &payload.attributes);
        Ok(())
    }

    pub(crate) fn end_span(&mut self, span_id: i64, payload_json: &str) -> Result<(), String> {
        if self.stack.last().copied() != Some(span_id) {
            if self.spans.contains_key(&span_id) {
                return Err(format!(
                    "guest span {span_id} cannot end before its active child"
                ));
            }
            return Err(format!("unknown guest span id: {span_id}"));
        }

        let payload: GuestSpanEndPayload = if payload_json.trim().is_empty() {
            GuestSpanEndPayload {
                status: Some("ok".to_string()),
                error_type: None,
                error_message: None,
                attributes: BTreeMap::new(),
            }
        } else {
            serde_json::from_str(payload_json)
                .map_err(|e| format!("invalid guest span end payload: {e}"))?
        };

        let entry = self
            .spans
            .remove(&span_id)
            .ok_or_else(|| format!("unknown guest span id: {span_id}"))?;
        self.stack.pop();
        let mut entry = entry;
        merge_allowed_attributes(&mut entry.attributes, &payload.attributes);
        merge_end_status_attributes(&mut entry.attributes, &payload);
        apply_attributes(&entry.span, &payload.attributes);
        apply_end_status(&entry.span, &payload);
        exit_thread_local_guest_span();
        end_otel_span(&entry.span);
        if self.manual_export {
            export_manual_span(&entry, status_from_end_payload(&payload), SystemTime::now());
        }
        drop(entry);
        Ok(())
    }

    pub(crate) fn enter_active(&self) -> Option<tracing::span::EnteredSpan> {
        let span_id = self.stack.last()?;
        self.spans
            .get(span_id)
            .map(|entry| entry.span.clone().entered())
    }

    pub(crate) fn cleanup_unclosed(&mut self) {
        while let Some(span_id) = self.stack.pop() {
            if let Some(entry) = self.spans.remove(&span_id) {
                entry.span.record("error.type", "unclosed_guest_span");
                entry.span.record("error.message", "guest span left open");
                entry
                    .span
                    .record("exception.message", "guest span left open");
                entry
                    .span
                    .context()
                    .span()
                    .set_status(Status::error("guest span left open"));
                exit_thread_local_guest_span();
                end_otel_span(&entry.span);
                if self.manual_export {
                    export_manual_span(
                        &entry,
                        Status::error("guest span left open"),
                        SystemTime::now(),
                    );
                }
                drop(entry);
            }
        }
        self.spans.clear();
        clear_thread_local_guest_spans();
    }

    fn span_mut(&mut self, span_id: i64) -> Result<&mut GuestSpanEntry, String> {
        self.spans
            .get_mut(&span_id)
            .ok_or_else(|| format!("unknown guest span id: {span_id}"))
    }
}

pub(crate) fn guest_span_attribute_allowed(key: &str) -> bool {
    let key = key.trim();
    if key.is_empty() {
        return false;
    }
    !matches!(
        key,
        "otel.name"
            | "otel.kind"
            | "trace_id"
            | "span_id"
            | "parent_span_id"
            | "dd.trace_id"
            | "dd.span_id"
            | "otel.trace_id"
            | "otel.span_id"
            | "_otel.parent_trace_id"
            | "_otel.parent_span_id"
    ) && !key.starts_with("_otel.")
}

fn update_span_name(span: &tracing::Span, name: &str) {
    span.record("otel.name", name);
    span.context().span().update_name(name.to_string());
}

fn enter_thread_local_guest_span(span: &tracing::Span) {
    ENTERED_GUEST_SPANS.with(|guards| {
        guards.borrow_mut().push(span.clone().entered());
    });
}

fn exit_thread_local_guest_span() {
    ENTERED_GUEST_SPANS.with(|guards| {
        let _ = guards.borrow_mut().pop();
    });
}

fn clear_thread_local_guest_spans() {
    ENTERED_GUEST_SPANS.with(|guards| guards.borrow_mut().clear());
}

fn manual_parent_context() -> Option<opentelemetry::Context> {
    let context = tracing::Span::current().context();
    let span_context = context.span().span_context().clone();
    span_context
        .is_valid()
        .then(|| opentelemetry::Context::new().with_remote_span_context(span_context))
}

fn tracing_span_ids(span: &tracing::Span) -> (Option<TraceId>, Option<SpanId>) {
    let context = span.context();
    let span_context = context.span().span_context().clone();
    if span_context.is_valid() {
        (Some(span_context.trace_id()), Some(span_context.span_id()))
    } else {
        (None, None)
    }
}

fn allowed_attributes(attributes: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    attributes
        .iter()
        .filter(|(key, _)| guest_span_attribute_allowed(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn manual_span_attributes(
    context: &WasmInvocationContext,
    span_id: i64,
    attributes: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    let mut manual = allowed_attributes(attributes);
    manual.insert("tenant".to_string(), Value::String(context.tenant.clone()));
    manual.insert(
        "entity_type".to_string(),
        Value::String(context.entity_type.clone()),
    );
    manual.insert(
        "entity_id".to_string(),
        Value::String(context.entity_id.clone()),
    );
    manual.insert(
        "trigger_action".to_string(),
        Value::String(context.trigger_action.clone()),
    );
    manual.insert(
        "wasm_module".to_string(),
        Value::String(context.wasm_module.clone().unwrap_or_default()),
    );
    manual.insert(
        "agent_id".to_string(),
        Value::String(context.agent_id.clone().unwrap_or_default()),
    );
    manual.insert(
        "session_id".to_string(),
        Value::String(context.session_id.clone().unwrap_or_default()),
    );
    manual.insert(
        "workflow_root_entity_type".to_string(),
        Value::String(
            context
                .workflow_root_entity_type
                .clone()
                .unwrap_or_default(),
        ),
    );
    manual.insert(
        "workflow_root_entity_id".to_string(),
        Value::String(context.workflow_root_entity_id.clone().unwrap_or_default()),
    );
    manual.insert(
        "workflow_run_id".to_string(),
        Value::String(context.workflow_run_id.clone().unwrap_or_default()),
    );
    manual.insert(
        "observability_event".to_string(),
        Value::String("wasm_guest.span".to_string()),
    );
    manual.insert("wasm_guest_span_id".to_string(), Value::from(span_id));
    manual
}

fn merge_allowed_attributes(
    target: &mut BTreeMap<String, Value>,
    attributes: &BTreeMap<String, Value>,
) {
    for (key, value) in attributes {
        if guest_span_attribute_allowed(key) {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn merge_end_status_attributes(
    target: &mut BTreeMap<String, Value>,
    payload: &GuestSpanEndPayload,
) {
    let status = payload.status.as_deref().unwrap_or("ok");
    if !status.eq_ignore_ascii_case("error") {
        return;
    }
    let error_message = payload
        .error_message
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("guest span failed");
    let error_type = payload
        .error_type
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("guest_span_error");
    target.insert(
        "error.type".to_string(),
        Value::String(error_type.to_string()),
    );
    target.insert(
        "error.message".to_string(),
        Value::String(error_message.to_string()),
    );
    target.insert(
        "exception.message".to_string(),
        Value::String(error_message.to_string()),
    );
}

fn status_from_end_payload(payload: &GuestSpanEndPayload) -> Status {
    let status = payload.status.as_deref().unwrap_or("ok");
    if status.eq_ignore_ascii_case("error") {
        let error_message = payload
            .error_message
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("guest span failed");
        Status::error(error_message.to_string())
    } else {
        Status::Ok
    }
}

fn export_manual_span(entry: &GuestSpanEntry, status: Status, end_time: SystemTime) {
    let (Some(trace_id), Some(span_id), Some(parent_context)) =
        (entry.trace_id, entry.span_id, entry.parent_context.clone())
    else {
        return;
    };

    let tracer = opentelemetry::global::tracer("temper-wasm-guest");
    let attrs = entry
        .attributes
        .iter()
        .filter_map(|(key, value)| key_value_from_json(key, value))
        .collect::<Vec<_>>();
    let events = entry
        .events
        .iter()
        .map(|event| {
            let attrs = event
                .attributes
                .iter()
                .filter_map(|(key, value)| key_value_from_json(key, value))
                .collect::<Vec<_>>();
            Event::new(event.name.clone(), event.timestamp, attrs, 0)
        })
        .collect::<Vec<_>>();
    let mut span = tracer.build_with_context(
        tracer
            .span_builder(entry.name.clone())
            .with_trace_id(trace_id)
            .with_span_id(span_id)
            .with_kind(span_kind(&entry.kind))
            .with_start_time(entry.start_time)
            .with_end_time(end_time)
            .with_status(status)
            .with_attributes(attrs)
            .with_events(events),
        &parent_context,
    );
    span.end_with_timestamp(end_time);
}

fn span_kind(kind: &str) -> SpanKind {
    match kind.to_ascii_lowercase().as_str() {
        "server" => SpanKind::Server,
        "client" => SpanKind::Client,
        "producer" => SpanKind::Producer,
        "consumer" => SpanKind::Consumer,
        _ => SpanKind::Internal,
    }
}

fn apply_attributes(span: &tracing::Span, attributes: &BTreeMap<String, Value>) {
    for (key, value) in attributes {
        let Some(kv) = key_value_from_json(key, value) else {
            continue;
        };
        if let Some(field_name) = datadog_visible_span_hint_field(key) {
            let value = span_record_value(value);
            span.record(field_name, value.as_str());
        }
        span.context().span().set_attribute(kv);
    }
}

fn apply_end_status(span: &tracing::Span, payload: &GuestSpanEndPayload) {
    let status = payload.status.as_deref().unwrap_or("ok");
    if status.eq_ignore_ascii_case("error") {
        let error_message = payload
            .error_message
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("guest span failed");
        let error_type = payload
            .error_type
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("guest_span_error");
        span.record("error.type", error_type);
        span.record("error.message", error_message);
        span.record("exception.message", error_message);
        span.context()
            .span()
            .set_status(Status::error(error_message.to_string()));
    } else {
        span.context().span().set_status(Status::Ok);
    }
}

fn end_otel_span(span: &tracing::Span) {
    let context = span.context();
    let otel_span = context.span();
    if otel_span.span_context().is_valid() {
        otel_span.end_with_timestamp(SystemTime::now());
    }
}

fn key_value_from_json(key: &str, value: &Value) -> Option<KeyValue> {
    if !guest_span_attribute_allowed(key) {
        return None;
    }
    let key = key.to_string();
    Some(match value {
        Value::String(value) => KeyValue::new(key, truncate_for_span_attr(value)),
        Value::Bool(value) => KeyValue::new(key, *value),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                KeyValue::new(key, value)
            } else if let Some(value) = value.as_u64().and_then(|value| i64::try_from(value).ok()) {
                KeyValue::new(key, value)
            } else if let Some(value) = value.as_f64() {
                KeyValue::new(key, value)
            } else {
                KeyValue::new(key, truncate_for_span_attr(&value.to_string()))
            }
        }
        Value::Null | Value::Array(_) | Value::Object(_) => {
            KeyValue::new(key, truncate_for_span_attr(&value.to_string()))
        }
    })
}

fn span_record_value(value: &Value) -> String {
    match value {
        Value::String(value) => truncate_for_span_attr(value),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            truncate_for_span_attr(&value.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
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
}
