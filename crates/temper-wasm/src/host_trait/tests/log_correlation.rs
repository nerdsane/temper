use super::*;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::sync::{Arc, Mutex};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::field::Visit;

#[test]
fn guest_log_named_event_carries_datadog_trace_and_span_correlation() {
    #[derive(Clone)]
    struct EventCapture {
        fields: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    }

    #[derive(Default)]
    struct FieldVisitor {
        fields: BTreeMap<String, String>,
    }

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    impl<S> tracing_subscriber::Layer<S> for EventCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().name() != "wasm_guest.log" {
                return;
            }
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.fields
                .lock()
                .expect("event capture lock poisoned")
                .push(visitor.fields);
        }
    }

    let tracer_provider = SdkTracerProvider::builder().build();
    let captured_fields = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("temper-wasm-test")),
        )
        .with(EventCapture {
            fields: captured_fields.clone(),
        });
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let context = WasmInvocationContext {
        tenant: "tenant-a".to_string(),
        entity_type: "Session".to_string(),
        entity_id: "ss-1".to_string(),
        trigger_action: "ContextReady".to_string(),
        wasm_module: Some("provider_caller".to_string()),
        trigger_params: serde_json::Value::Null,
        entity_state: serde_json::Value::Null,
        agent_id: Some("agent-1".to_string()),
        session_id: None,
        integration_config: BTreeMap::new(),
        trace_id: "0123456789abcdef0123456789abcdef".to_string(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: None,
    };

    let span = tracing::info_span!("wasm.invoke");
    let (expected_trace_id, expected_span_id) = span.in_scope(|| {
        let span_context = tracing::Span::current()
            .context()
            .span()
            .span_context()
            .clone();
        assert!(
            span_context.is_valid(),
            "test span should have an OTEL context"
        );
        (
            span_context.trace_id().to_string(),
            span_context.span_id().to_string(),
        )
    });
    let expected_dd_trace_id = u64::from_str_radix(&expected_trace_id[16..], 16)
        .expect("trace id low bits should parse")
        .to_string();
    let expected_dd_span_id = u64::from_str_radix(&expected_span_id, 16)
        .expect("span id should parse")
        .to_string();

    span.in_scope(|| {
        record_guest_log_span_event("info", "provider finished", Some(&context), None);
    });

    let fields = captured_fields.lock().expect("event capture lock poisoned");
    let event = fields
        .first()
        .expect("expected a wasm_guest.log event to be captured");
    assert_eq!(event["guest_log.message"], "provider finished");
    assert_eq!(event["session_id"], "ss-1");
    assert_eq!(event["gen_ai.conversation.id"], "ss-1");
    assert_eq!(event["trace_id"], expected_trace_id);
    assert_eq!(event["span_id"], expected_span_id);
    assert_eq!(event["otel.trace_id"], expected_trace_id);
    assert_eq!(event["otel.span_id"], expected_span_id);
    assert_eq!(event["dd.trace_id"], expected_dd_trace_id);
    assert_eq!(event["dd.span_id"], expected_dd_span_id);
}
