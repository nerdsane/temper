//! Upload boundaries retain deferred execution and poll-time tracing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use temper_runtime::ActorSystem;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Instrument, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use super::*;
use crate::registry::SpecRegistry;

#[derive(Clone, Default)]
struct UploadTrace(Arc<Mutex<Vec<RecordedSpan>>>);

#[derive(Debug)]
struct RecordedSpan {
    name: &'static str,
    parent: Option<&'static str>,
    fields: BTreeMap<String, String>,
    declared_fields: BTreeSet<&'static str>,
}

struct Fields<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S> Layer<S> for UploadTrace
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, ctx: Context<'_, S>) {
        let name = attrs.metadata().name();
        if !name.starts_with("put_file_stream_content") {
            return;
        }
        let parent = if let Some(parent) = attrs.parent() {
            ctx.span(parent)
        } else if attrs.is_contextual() {
            ctx.lookup_current()
        } else {
            None
        }
        .map(|span| span.metadata().name());
        let mut fields = BTreeMap::new();
        attrs.record(&mut Fields(&mut fields));
        self.0.lock().unwrap().push(RecordedSpan {
            name,
            parent,
            fields,
            declared_fields: attrs
                .metadata()
                .fields()
                .iter()
                .map(|field| field.name())
                .collect(),
        });
    }
}

fn state() -> ServerState {
    ServerState::from_registry(
        ActorSystem::new("file-stream-boundary-test"),
        SpecRegistry::new(),
    )
}

fn assert_send<T: Send>(_: &T) {}

#[tokio::test]
async fn dropping_unpolled_upload_futures_creates_no_actor_event_or_span() {
    let state = state();
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    let mut events = state.event_tx.subscribe();
    let trace = UploadTrace::default();
    let subscriber = tracing_subscriber::registry().with(trace.clone());

    tracing::subscriber::with_default(subscriber, || {
        let checked = state.put_file_stream_content_checked(
            &tenant,
            "unregistered-file",
            b"payload",
            "text/plain",
            &agent,
            Some("unchanged-precondition".to_string()),
        );
        assert_send(&checked);
        let public = state.put_file_stream_content(
            &tenant,
            "unregistered-file",
            b"payload",
            "text/plain",
            &agent,
        );
        assert_send(&public);
        assert!(trace.0.lock().unwrap().is_empty());
        assert!(state.actor_registry.read().unwrap().is_empty());
        drop(checked);
        drop(public);
    });

    assert!(trace.0.lock().unwrap().is_empty());
    assert!(state.actor_registry.read().unwrap().is_empty());
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn polled_checked_upload_preserves_span_name_fields_and_parentage() {
    use tracing::instrument::WithSubscriber;

    let state = state();
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    let trace = UploadTrace::default();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(trace.clone()));
    let caller =
        tracing::dispatcher::with_default(&dispatch, || tracing::info_span!("upload_caller"));

    let result = state
        .put_file_stream_content_checked(
            &tenant,
            "unregistered-file",
            b"payload",
            "text/plain",
            &agent,
            None,
        )
        .instrument(caller)
        .with_subscriber(dispatch)
        .await;
    assert!(matches!(result, Err(FileStreamContentError::State(_))));

    let spans = trace.0.lock().unwrap();
    let checked: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "put_file_stream_content_checked")
        .collect();
    assert_eq!(checked.len(), 1, "retain the original checked-upload span");
    let checked = checked[0];
    assert_eq!(checked.parent, Some("upload_caller"));
    for (field, value) in [
        ("tenant", "default"),
        ("request_bytes", "7"),
        ("otel.name", "state.put_file_stream_content"),
    ] {
        assert_eq!(checked.fields.get(field).map(String::as_str), Some(value));
    }
    // The original `skip_all, fields(file_id, ...)` span declares this field
    // without assigning its argument. Preserve that format, not a new value.
    assert!(checked.declared_fields.contains("file_id"));
    assert!(!checked.fields.contains_key("file_id"));
}

#[tokio::test]
async fn polled_public_upload_preserves_checked_span_and_error_mapping() {
    use tracing::instrument::WithSubscriber;

    let state = state();
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    // Obtain the typed reference before creating this observation's subscriber;
    // only the public call below belongs in its span-count assertion.
    let checked_error = state
        .put_file_stream_content_checked(
            &tenant,
            "unregistered-file",
            b"payload",
            "text/plain",
            &agent,
            None,
        )
        .await
        .unwrap_err();
    let trace = UploadTrace::default();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(trace.clone()));
    let future = tracing::dispatcher::with_default(&dispatch, || {
        state.put_file_stream_content(
            &tenant,
            "unregistered-file",
            b"payload",
            "text/plain",
            &agent,
        )
    });
    assert_send(&future);
    assert!(trace.0.lock().unwrap().is_empty());
    let caller =
        tracing::dispatcher::with_default(&dispatch, || tracing::info_span!("upload_caller"));
    let error = future
        .instrument(caller)
        .with_subscriber(dispatch)
        .await
        .unwrap_err();
    assert!(matches!(&checked_error, FileStreamContentError::State(_)));
    assert_eq!(error, checked_error.to_string());

    let spans = trace.0.lock().unwrap();
    assert_eq!(
        spans.len(),
        2,
        "public wrapper must not add another upload span"
    );
    let checked = spans
        .iter()
        .find(|span| span.name == "put_file_stream_content_checked")
        .expect("original checked-upload span");
    assert_eq!(checked.parent, Some("upload_caller"));
    assert_eq!(
        checked.fields.get("otel.name").map(String::as_str),
        Some("state.put_file_stream_content")
    );
    assert_eq!(
        checked.fields.get("request_bytes").map(String::as_str),
        Some("7")
    );
    assert!(checked.declared_fields.contains("file_id"));
    assert!(!checked.fields.contains_key("file_id"));
    let native = spans
        .iter()
        .find(|span| span.name == "put_file_stream_content_native")
        .expect("original native-upload child span");
    assert_eq!(native.parent, Some("put_file_stream_content_checked"));
    assert_eq!(
        native.fields.get("otel.name").map(String::as_str),
        Some("state.put_file_stream_content.native")
    );
    assert_eq!(
        native.fields.get("tenant").map(String::as_str),
        Some("default")
    );
    assert_eq!(
        native.fields.get("request_bytes").map(String::as_str),
        Some("7")
    );
    assert!(native.declared_fields.contains("file_id"));
    assert!(!native.fields.contains_key("file_id"));
}
