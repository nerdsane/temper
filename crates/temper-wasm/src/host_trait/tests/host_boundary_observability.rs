use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use tracing::field::{Field, Visit};
use tracing_subscriber::prelude::*;

use crate::{ProductionWasmHost, WasmHost, WasmInvocationContext};

#[derive(Clone, Debug, Default)]
struct CapturedSpan {
    name: String,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default)]
struct CapturedEvent {
    name: String,
    fields: BTreeMap<String, String>,
}

type CapturedSpans = Arc<Mutex<Vec<CapturedSpan>>>;
type CapturedEvents = Arc<Mutex<Vec<CapturedEvent>>>;
type CaptureGuard = tracing::dispatcher::DefaultGuard;

#[derive(Default)]
struct CaptureVisitor {
    fields: BTreeMap<String, String>,
}

impl Visit for CaptureVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[derive(Clone)]
struct SpanEventCapture {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
    span_indices: Arc<Mutex<BTreeMap<u64, usize>>>,
}

impl<S> tracing_subscriber::Layer<S> for SpanEventCapture
where
    S: tracing::Subscriber,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = CaptureVisitor::default();
        attrs.record(&mut visitor);
        let mut spans = self.spans.lock().expect("span capture lock poisoned");
        let index = spans.len();
        spans.push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields: visitor.fields,
        });
        self.span_indices
            .lock()
            .expect("span index lock poisoned")
            .insert(id.into_u64(), index);
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = CaptureVisitor::default();
        values.record(&mut visitor);
        let Some(index) = self
            .span_indices
            .lock()
            .expect("span index lock poisoned")
            .get(&id.into_u64())
            .copied()
        else {
            return;
        };
        if let Some(span) = self
            .spans
            .lock()
            .expect("span capture lock poisoned")
            .get_mut(index)
        {
            span.fields.extend(visitor.fields);
        }
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = CaptureVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("event capture lock poisoned")
            .push(CapturedEvent {
                name: event.metadata().name().to_string(),
                fields: visitor.fields,
            });
    }
}

fn capture_spans_and_events() -> (CapturedSpans, CapturedEvents, CaptureGuard) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let layer = SpanEventCapture {
        spans: spans.clone(),
        events: events.clone(),
        span_indices: Arc::new(Mutex::new(BTreeMap::new())),
    };
    let subscriber = tracing_subscriber::registry().with(layer);
    let guard = tracing::subscriber::set_default(subscriber);
    (spans, events, guard)
}

fn session_invocation_context() -> WasmInvocationContext {
    WasmInvocationContext {
        tenant: "tenant-a".to_string(),
        entity_type: "Session".to_string(),
        entity_id: "ss-1".to_string(),
        trigger_action: "CallProvider".to_string(),
        wasm_module: Some("provider_caller".to_string()),
        trigger_params: serde_json::Value::Null,
        entity_state: serde_json::Value::Null,
        agent_id: Some("agent-1".to_string()),
        session_id: Some("ss-1".to_string()),
        integration_config: BTreeMap::new(),
        trace_id: "0123456789abcdef0123456789abcdef".to_string(),
        workflow_root_entity_type: Some("ManagedSession".to_string()),
        workflow_root_entity_id: Some("ms-1".to_string()),
        workflow_run_id: Some("ManagedSession:ms-1".to_string()),
        http_request: None,
    }
}

#[test]
fn secret_lookup_emits_contextual_host_span_without_secret_value() {
    let (spans, _events, _guard) = capture_spans_and_events();
    let mut secrets = BTreeMap::new();
    secrets.insert("provider_api_key".to_string(), "secret-value".to_string());
    let host =
        ProductionWasmHost::new(secrets).with_invocation_context(session_invocation_context());

    let root = tracing::info_span!("wasm.invoke");
    let value = root.in_scope(|| host.get_secret("provider_api_key"));
    assert_eq!(value, Ok("secret-value".to_string()));

    let spans = spans.lock().expect("span capture lock poisoned");
    let span = spans
        .iter()
        .find(|span| span.name == "wasm.host.get_secret")
        .expect("secret lookup should emit a host-function span");

    assert_eq!(
        span.fields.get("secret.key").map(String::as_str),
        Some("provider_api_key")
    );
    assert_eq!(span.fields.get("success").map(String::as_str), Some("true"));
    assert_eq!(
        span.fields.get("tenant").map(String::as_str),
        Some("tenant-a")
    );
    assert_eq!(
        span.fields.get("entity_id").map(String::as_str),
        Some("ss-1")
    );
    assert_eq!(
        span.fields.get("session_id").map(String::as_str),
        Some("ss-1")
    );
    assert_eq!(
        span.fields.get("action_name").map(String::as_str),
        Some("CallProvider")
    );
    assert_eq!(
        span.fields.get("wasm_module").map(String::as_str),
        Some("provider_caller")
    );
    assert!(
        !span
            .fields
            .values()
            .any(|value| value.contains("secret-value")),
        "secret span must never record the secret value"
    );
}

#[test]
fn progress_emission_creates_correlated_guest_event() {
    let (_spans, events, _guard) = capture_spans_and_events();
    let host = ProductionWasmHost::new(BTreeMap::new())
        .with_invocation_context(session_invocation_context())
        .with_progress_emitter(Arc::new(|_| Ok(())));

    let root = tracing::info_span!("wasm.invoke");
    let result = root.in_scope(|| {
        host.emit_progress(
            r#"{"kind":"tool_progress","workflow_step":"tool_batch","tool_name":"bash"}"#,
        )
    });
    assert_eq!(result, Ok(()));

    let events = events.lock().expect("event capture lock poisoned");
    let event = events
        .iter()
        .find(|event| event.name == "wasm_guest.progress")
        .expect("progress emission should create a Datadog-searchable event");

    assert_eq!(
        event.fields.get("tenant").map(String::as_str),
        Some("tenant-a")
    );
    assert_eq!(
        event.fields.get("entity_id").map(String::as_str),
        Some("ss-1")
    );
    assert_eq!(
        event.fields.get("session_id").map(String::as_str),
        Some("ss-1")
    );
    assert_eq!(
        event.fields.get("wasm_module").map(String::as_str),
        Some("provider_caller")
    );
    assert_eq!(
        event.fields.get("progress.kind").map(String::as_str),
        Some("tool_progress")
    );
    assert_eq!(
        event.fields.get("workflow_step").map(String::as_str),
        Some("tool_batch")
    );
    assert_eq!(
        event.fields.get("success").map(String::as_str),
        Some("true")
    );
}
