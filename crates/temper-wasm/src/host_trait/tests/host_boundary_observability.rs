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
        trigger_principal: None,
    }
}

/// The shapes a secret in a URL takes. None may reach a span, a log line or the module's error.
const KEYS: [&str; 6] = [
    // Katagami's interface builder's address.
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    // A Slack webhook's last segment.
    "aB3dE6gH9jK2mN5pQ8sT1vW4",
    // A long key with no digit.
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJ",
    // A key whose tilde joins two individually short runs.
    "0123456789abcdef~fedcba9876543210",
    // A period joins two individually short runs into one path key.
    "0123456789abcdef.fedcba9876543210",
    // 32 hex, which also parses as a UUID with its hyphens left out.
    "fedcba9876543210fedcba9876543210",
];

fn assert_no_key(key: &str, spans: &CapturedSpans, events: &CapturedEvents, also: &str) {
    assert!(!also.contains(key), "the module was told {key}: {also}");
    for span in spans.lock().expect("span capture lock poisoned").iter() {
        for (name, value) in &span.fields {
            assert!(
                !value.contains(key),
                "span {} field {name} holds {key}: {value}",
                span.name
            );
        }
    }
    for event in events.lock().expect("event capture lock poisoned").iter() {
        for (name, value) in &event.fields {
            assert!(
                !value.contains(key),
                "log {} field {name} holds {key}: {value}",
                event.name
            );
        }
    }
}

#[tokio::test]
async fn a_key_in_a_called_url_reaches_no_span_log_or_error() {
    for key in KEYS {
        let (spans, events, _guard) = capture_spans_and_events();
        let host = ProductionWasmHost::new(BTreeMap::new());

        // A call that answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let up = listener.local_addr().expect("address");
        let answering = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });
        let url = format!("http://{up}/{key}/build?token={key}#{key}");
        assert_eq!(
            host.http_call("POST", &url, &[], "{}")
                .await
                .expect("answered")
                .0,
            200
        );
        assert_eq!(
            host.http_call_binary("POST", &url, &[], b"{}")
                .await
                .expect("answered")
                .0,
            200
        );
        answering.await.expect("server");

        // A call that fails: reqwest's error names the URL it was sent to.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let down = closed.local_addr().expect("address");
        drop(closed);
        let url = format!("http://{down}/{key}/build?token={key}");
        let refused = host
            .http_call("POST", &url, &[], "{}")
            .await
            .expect_err("refused");
        let refused_binary = host
            .http_call_binary("POST", &url, &[], b"{}")
            .await
            .expect_err("refused");

        let captured = spans.lock().expect("span capture lock poisoned").clone();
        let url_field = captured
            .iter()
            .find(|span| span.name == "wasm.host.http_call")
            .and_then(|span| span.fields.get("http.url"))
            .expect("the call's span names its URL");
        assert!(url_field.ends_with("/<redacted>/build"), "{url_field}");
        assert!(url_field.starts_with("127.0.0.1:"), "{url_field}");
        assert_no_key(key, &spans, &events, &format!("{refused} {refused_binary}"));
    }
}

/// The stream path is where the interface builder's key was seen, on 1 Nov 2026.
#[tokio::test]
async fn a_key_in_a_streamed_call_reaches_no_span_log_or_response_head() {
    for key in KEYS {
        let (spans, events, _guard) = capture_spans_and_events();
        let host = ProductionWasmHost::new(BTreeMap::new());
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let down = closed.local_addr().expect("address");
        drop(closed);
        let handles = host
            .http_stream_begin_outbound(crate::http_stream::HttpRequestHead {
                method: "POST".to_string(),
                url: format!("http://{down}/{key}/build?token={key}"),
                headers: Vec::new(),
            })
            .await
            .expect("open outbound exchange");
        host.http_stream_try_write(handles.request_body, b"{}".to_vec())
            .await
            .expect("queue the request");
        let head = host
            .http_stream_response_head(handles.response_body)
            .await
            .expect("receive the transport failure");
        assert_eq!(head.status, 0);

        let url_field = spans
            .lock()
            .expect("span capture lock poisoned")
            .iter()
            .find(|span| span.name == "wasm.host.http_stream")
            .and_then(|span| span.fields.get("http.url").cloned())
            .expect("the call's span names its URL");
        assert!(url_field.ends_with("/<redacted>/build"), "{url_field}");
        assert_no_key(key, &spans, &events, &format!("{:?}", head.headers));
    }
}

#[test]
fn a_url_in_telemetry_keeps_what_a_reader_needs() {
    use crate::host_trait::telemetry_url as seen;
    // Words and ids a reader joins on stay.
    assert_eq!(
        seen("https://api.tensorlake.ai/artifact-storage/v1/token"),
        "api.tensorlake.ai/artifact-storage/v1/token"
    );
    assert_eq!(
        seen("http://h/tdata/KatagamiAppSurfaces('a4n0fypl22sn.sumxn1gvgq7dh')/KatagamiApps.Kept"),
        "h/tdata/KatagamiAppSurfaces('a4n0fypl22sn.sumxn1gvgq7dh')/KatagamiApps.Kept"
    );
    assert_eq!(
        seen("http://h/x/01a19923-dd62-7fd4-9e8c-ea29db7c3dba/y"),
        "h/x/01a19923-dd62-7fd4-9e8c-ea29db7c3dba/y"
    );
    // A key goes, wherever in the path it sits, and only the key.
    assert_eq!(
        seen("https://user:pw@h/services/T000/B000/aB3dE6gH9jK2mN5pQ8sT1vW4?x=1#y"),
        "h/services/T000/B000/<redacted>"
    );
    assert_eq!(
        seen("http://h/bot123456789:AAE-abcdefghijklmnopqrstuvwxyz012345/sendMessage"),
        "h/bot123456789:<redacted>/sendMessage"
    );
    assert_eq!(
        seen("https://h/0123456789abcdef.fedcba9876543210/build"),
        "h/<redacted>/build"
    );
    // Entity expressions do not exempt an individual key-shaped component.
    assert_eq!(
        seen("https://h/tdata/Entities('0123456789abcdef0123456789abcdef.short')/Kept"),
        "h/tdata/Entities('<redacted>')/Kept"
    );
    assert_eq!(seen("http://h"), "h");
    for suffix in [
        "/build?account=user@0123456789abcdef0123456789abcdef",
        "/build#user@0123456789abcdef0123456789abcdef",
        "/user@0123456789abcdef0123456789abcdef/build",
    ] {
        let expected = if suffix.starts_with("/user@") {
            "h/user@<redacted>/build"
        } else {
            "h/build"
        };
        assert_eq!(seen(&format!("https://user:pw@h{suffix}")), expected);
        assert_eq!(seen(&format!("https://h{suffix}")), expected);
    }
    assert_eq!(seen("https://h?account=user@secret"), "h");
    assert_eq!(seen("https://h#user@secret"), "h");
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
