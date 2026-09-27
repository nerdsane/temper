//! Dispatch boundaries retain deferred execution and poll-time tracing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::json;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Instrument, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use super::*;
use crate::state::dispatch::strict_test_support;

#[derive(Clone, Default)]
struct DispatchTrace(Arc<Mutex<BTreeMap<u64, RecordedSpan>>>);

#[derive(Debug)]
struct RecordedSpan {
    name: &'static str,
    parent: Option<&'static str>,
    declared_fields: BTreeSet<&'static str>,
    fields: BTreeMap<String, String>,
    entries: usize,
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

impl<S> Layer<S> for DispatchTrace
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let name = attrs.metadata().name();
        if !matches!(
            name,
            "dispatch" | "dispatch_tenant_action" | "dispatch_tenant_action_core"
        ) {
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
        self.0.lock().unwrap().insert(
            id.into_u64(),
            RecordedSpan {
                name,
                parent,
                declared_fields: attrs
                    .metadata()
                    .fields()
                    .iter()
                    .map(|field| field.name())
                    .collect(),
                fields,
                entries: 0,
            },
        );
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        if let Some(span) = self.0.lock().unwrap().get_mut(&id.into_u64()) {
            values.record(&mut Fields(&mut span.fields));
        }
    }

    fn on_enter(&self, id: &Id, _ctx: Context<'_, S>) {
        if let Some(span) = self.0.lock().unwrap().get_mut(&id.into_u64()) {
            span.entries += 1;
        }
    }
}

fn command<'a>(tenant: &'a TenantId, agent: &'a AgentContext) -> DispatchCommand<'a> {
    DispatchCommand {
        tenant,
        entity_type: "StrictJob",
        entity_id: "job",
        action: "Complete",
        params: json!({"observed": "accepted", "expected_revision": 1}),
        agent_ctx: agent,
        await_integration: true,
        await_reactions: true,
    }
}

fn assert_send<T: Send>(_: &T) {}

#[tokio::test]
async fn dropping_unpolled_dispatch_futures_creates_no_actor_event_or_span() {
    let state = strict_test_support::state();
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    let mut events = state.event_tx.subscribe();
    let trace = DispatchTrace::default();
    let subscriber = tracing_subscriber::registry().with(trace.clone());

    tracing::subscriber::with_default(subscriber, || {
        let public = state.dispatch(command(&tenant, &agent));
        assert_send(&public);
        let typed = state.dispatch_typed_checked(command(&tenant, &agent), None);
        assert_send(&typed);
        let core = state.dispatch_tenant_action_core(
            &tenant,
            "StrictJob",
            "job",
            "Complete",
            json!({"observed": "accepted", "expected_revision": 1}),
            &agent,
            true,
            None,
        );
        assert_send(&core);
        let tenant_dispatch = state.dispatch_tenant_action(
            &tenant,
            "StrictJob",
            "job",
            "Complete",
            json!({"observed": "accepted", "expected_revision": 1}),
            &agent,
        );
        assert_send(&tenant_dispatch);
        assert!(trace.0.lock().unwrap().is_empty());
        assert!(state.actor_registry.read().unwrap().is_empty());
        drop(public);
        drop(typed);
        drop(core);
        drop(tenant_dispatch);
    });

    assert!(trace.0.lock().unwrap().is_empty());
    assert!(state.actor_registry.read().unwrap().is_empty());
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

fn assert_fields(span: &RecordedSpan, otel_name: &str) {
    for (field, value) in [
        ("tenant", "default"),
        ("action_name", "Complete"),
        ("otel.name", otel_name),
    ] {
        assert_eq!(span.fields.get(field).map(String::as_str), Some(value));
    }
    assert!(
        span.entries > 0,
        "span must enter while the future is polled"
    );
}

fn assert_core_fields(span: &RecordedSpan) {
    assert_fields(span, "dispatch.dispatch_tenant_action_core");
    // The original core span declares these fields without assigning values.
    // Preserve that contract rather than changing telemetry for this boundary.
    for field in ["entity_type", "entity_id"] {
        assert!(span.declared_fields.contains(field));
        assert!(!span.fields.contains_key(field));
    }
    assert_eq!(span.fields.get("success").map(String::as_str), Some("true"));
}

#[tokio::test]
async fn tenant_dispatch_preserves_action_and_poll_time_span_parentage() {
    use tracing::instrument::WithSubscriber;

    let state = strict_test_support::state();
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    let mut events = state.event_tx.subscribe();
    let trace = DispatchTrace::default();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(trace.clone()));
    let future = tracing::dispatcher::with_default(&dispatch, || {
        state.dispatch_tenant_action(
            &tenant,
            "StrictJob",
            "job",
            "Complete",
            json!({"observed": "accepted", "expected_revision": 1}),
            &agent,
        )
    });
    assert_send(&future);
    assert!(trace.0.lock().unwrap().is_empty());
    let parent = tracing::dispatcher::with_default(&dispatch, || {
        tracing::info_span!("dispatch-boundary-request")
    });
    let response = future
        .instrument(parent)
        .with_subscriber(dispatch)
        .await
        .unwrap();
    assert!(response.success, "{response:?}");
    assert_eq!(response.state.status, "Done");
    assert_eq!(events.try_recv().unwrap().status, "Done");

    let spans = trace.0.lock().unwrap();
    assert_eq!(spans.len(), 3);
    let tenant_dispatch = spans
        .values()
        .find(|span| span.name == "dispatch_tenant_action")
        .unwrap();
    assert_eq!(tenant_dispatch.parent, Some("dispatch-boundary-request"));
    assert_fields(tenant_dispatch, "StrictJob.Complete");
    for field in ["entity_type", "entity_id"] {
        assert!(tenant_dispatch.declared_fields.contains(field));
        assert!(!tenant_dispatch.fields.contains_key(field));
    }
    let public = spans.values().find(|span| span.name == "dispatch").unwrap();
    assert_eq!(public.parent, Some("dispatch_tenant_action"));
    let core = spans
        .values()
        .find(|span| span.name == "dispatch_tenant_action_core")
        .unwrap();
    assert_eq!(core.parent, Some("dispatch"));
    assert_core_fields(core);
}

#[tokio::test]
async fn public_dispatch_preserves_action_and_poll_time_span_parentage() {
    use tracing::instrument::WithSubscriber;

    let state = strict_test_support::state();
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    let mut events = state.event_tx.subscribe();
    let trace = DispatchTrace::default();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(trace.clone()));
    let future =
        tracing::dispatcher::with_default(&dispatch, || state.dispatch(command(&tenant, &agent)));
    assert!(trace.0.lock().unwrap().is_empty());
    let parent = tracing::dispatcher::with_default(&dispatch, || {
        tracing::info_span!("dispatch-boundary-request")
    });
    let response = future
        .instrument(parent)
        .with_subscriber(dispatch)
        .await
        .unwrap();
    assert!(response.success, "{response:?}");
    assert_eq!(response.state.status, "Done");
    assert_eq!(events.try_recv().unwrap().status, "Done");

    let spans = trace.0.lock().unwrap();
    assert_eq!(spans.len(), 2);
    let public = spans.values().find(|span| span.name == "dispatch").unwrap();
    assert_eq!(public.parent, Some("dispatch-boundary-request"));
    assert_fields(public, "StrictJob.Complete");
    assert_eq!(
        public.fields.get("entity_type").map(String::as_str),
        Some("StrictJob")
    );
    assert_eq!(
        public.fields.get("entity_id").map(String::as_str),
        Some("job")
    );
    let core = spans
        .values()
        .find(|span| span.name == "dispatch_tenant_action_core")
        .unwrap();
    assert_eq!(core.parent, Some("dispatch"));
    assert_core_fields(core);
}

#[tokio::test]
async fn core_dispatch_preserves_action_and_poll_time_span_parentage() {
    use tracing::instrument::WithSubscriber;

    let state = strict_test_support::state();
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    let trace = DispatchTrace::default();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(trace.clone()));
    let future = tracing::dispatcher::with_default(&dispatch, || {
        state.dispatch_tenant_action_core(
            &tenant,
            "StrictJob",
            "job",
            "Complete",
            json!({"observed": "accepted", "expected_revision": 1}),
            &agent,
            true,
            None,
        )
    });
    assert!(trace.0.lock().unwrap().is_empty());
    let parent = tracing::dispatcher::with_default(&dispatch, || {
        tracing::info_span!("dispatch-boundary-request")
    });
    let response = future
        .instrument(parent)
        .with_subscriber(dispatch)
        .await
        .unwrap();
    assert!(response.success, "{response:?}");
    assert_eq!(response.state.status, "Done");

    let spans = trace.0.lock().unwrap();
    assert_eq!(spans.len(), 1);
    let core = spans.values().next().unwrap();
    assert_eq!(core.parent, Some("dispatch-boundary-request"));
    assert_core_fields(core);
}
