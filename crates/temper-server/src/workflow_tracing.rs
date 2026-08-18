use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use opentelemetry::trace::TraceContextExt;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

const WORKFLOW_ROOT_DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct WorkflowRootSpan {
    root_entity_type: String,
    root_entity_id: String,
    span: tracing::Span,
}

/// Keeps workflow root spans open across asynchronous entity actions.
#[derive(Debug, Default)]
pub(crate) struct WorkflowSpanRegistry {
    spans: Mutex<BTreeMap<String, WorkflowRootSpan>>,
}

impl WorkflowSpanRegistry {
    pub(crate) fn parent_context(
        &self,
        tenant: &str,
        root_entity_type: &str,
        root_entity_id: &str,
        workflow_run_id: &str,
        session_id: Option<&str>,
    ) -> Option<opentelemetry::Context> {
        let mut spans = self.spans.lock().ok()?;
        if let Some(root) = spans.get(workflow_run_id) {
            let context = root.span.context();
            if context.span().span_context().is_valid() {
                return Some(context);
            }
            spans.remove(workflow_run_id);
        }

        let span = tracing::info_span!(
            parent: None,
            "temper.workflow",
            otel.name = %format_args!("{root_entity_type}.workflow"),
            tenant = %tenant,
            entity_type = %root_entity_type,
            entity_id = %root_entity_id,
            workflow.root_entity_type = %root_entity_type,
            workflow.root_entity_id = %root_entity_id,
            workflow.run_id = %workflow_run_id,
            workflow.terminal_status = tracing::field::Empty,
            workflow.drain_grace_ms = tracing::field::Empty,
            workflow.drain_reason = tracing::field::Empty,
            session.id = session_id.unwrap_or(""),
        );
        let context = span.context();
        if !context.span().span_context().is_valid() {
            return None;
        }

        spans.insert(
            workflow_run_id.to_string(),
            WorkflowRootSpan {
                root_entity_type: root_entity_type.to_string(),
                root_entity_id: root_entity_id.to_string(),
                span,
            },
        );
        Some(context)
    }

    pub(crate) fn finish_if_terminal(
        &self,
        workflow_run_id: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
    ) -> bool {
        if !is_terminal_status(status) {
            return false;
        }

        let Ok(mut spans) = self.spans.lock() else {
            return false;
        };

        let should_finish = spans.get(workflow_run_id).is_some_and(|root| {
            root.root_entity_type == entity_type && root.root_entity_id == entity_id
        });
        if should_finish {
            spans.remove(workflow_run_id);
        }
        should_finish
    }

    fn mark_terminal_drain_scheduled(
        &self,
        workflow_run_id: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
    ) -> Option<opentelemetry::Context> {
        let spans = self.spans.lock().ok()?;
        let root = spans.get(workflow_run_id)?;
        if root.root_entity_type != entity_type || root.root_entity_id != entity_id {
            return None;
        }

        root.span.record("workflow.terminal_status", status);
        root.span.record(
            "workflow.drain_grace_ms",
            workflow_root_drain_grace_ms() as i64,
        );
        root.span
            .record("workflow.drain_reason", "post_dispatch_telemetry");

        let context = root.span.context();
        if context.span().span_context().is_valid() {
            Some(context)
        } else {
            None
        }
    }

    pub(crate) fn finish_if_terminal_after_drain(
        self: &std::sync::Arc<Self>,
        workflow_run_id: String,
        entity_type: String,
        entity_id: String,
        status: String,
    ) {
        if !is_terminal_status(&status) {
            return;
        }

        let registry = std::sync::Arc::clone(self);
        let drain_context = registry.mark_terminal_drain_scheduled(
            &workflow_run_id,
            &entity_type,
            &entity_id,
            &status,
        );
        tokio::spawn(async move {
            // determinism-ok: observability-only root span cleanup after
            // post-dispatch telemetry has had a chance to attach.
            if let Some(parent_context) = drain_context {
                let span = tracing::info_span!(
                    parent: None,
                    "temper.workflow.drain_grace",
                    otel.name = "temper.workflow.drain_grace",
                    workflow.run_id = %workflow_run_id,
                    workflow.root_entity_type = %entity_type,
                    workflow.root_entity_id = %entity_id,
                    workflow.terminal_status = %status,
                    workflow.drain_grace_ms = workflow_root_drain_grace_ms() as i64,
                    workflow.drain_reason = "post_dispatch_telemetry",
                );
                span.set_parent(parent_context);
                async {
                    tokio::time::sleep(WORKFLOW_ROOT_DRAIN_GRACE).await;
                }
                .instrument(span)
                .await;
            } else {
                tokio::time::sleep(WORKFLOW_ROOT_DRAIN_GRACE).await;
            }
            registry.finish_if_terminal(&workflow_run_id, &entity_type, &entity_id, &status);
        });
    }
}

fn workflow_root_drain_grace_ms() -> u64 {
    WORKFLOW_ROOT_DRAIN_GRACE.as_millis() as u64
}

pub(crate) fn should_use_workflow_root_span(
    explicit_workflow_context: bool,
    entity_type: &str,
) -> bool {
    explicit_workflow_context
        || matches!(
            entity_type,
            "Session" | "ManagedSession" | "WorkCycle" | "AlertCycle" | "TemperAgent"
        )
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "Completed" | "Failed" | "Cancelled" | "Terminated")
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::TraceContextExt;
    use opentelemetry::trace::TracerProvider as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::prelude::*;

    #[test]
    fn workflow_root_span_is_not_child_of_short_http_request_span() {
        let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer_provider.tracer("temper-server-workflow-root-test")),
        );
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let registry = super::WorkflowSpanRegistry::default();
        let http_span = tracing::info_span!("http.request");
        let http_trace_id = http_span
            .context()
            .span()
            .span_context()
            .trace_id()
            .to_string();

        let workflow_parent = http_span.in_scope(|| {
            registry.parent_context("default", "Session", "ss-1", "Session:ss-1", Some("ss-1"))
        });
        let workflow_parent = workflow_parent.expect("workflow root context");
        let workflow_trace_id = workflow_parent.span().span_context().trace_id().to_string();
        assert_ne!(workflow_trace_id, http_trace_id);

        http_span.in_scope(|| {
            let dispatch_span = tracing::info_span!("dispatch.dispatch_tenant_action_core");
            dispatch_span.set_parent(workflow_parent);
            let dispatch_trace_id = dispatch_span
                .context()
                .span()
                .span_context()
                .trace_id()
                .to_string();
            assert_eq!(dispatch_trace_id, workflow_trace_id);
        });

        assert!(registry.finish_if_terminal("Session:ss-1", "Session", "ss-1", "Completed"));
    }

    #[test]
    fn workflow_root_span_is_only_used_for_workflow_context_or_agent_cycles() {
        assert!(super::should_use_workflow_root_span(false, "Session"));
        assert!(super::should_use_workflow_root_span(
            false,
            "ManagedSession"
        ));
        assert!(super::should_use_workflow_root_span(false, "WorkCycle"));
        assert!(super::should_use_workflow_root_span(false, "AlertCycle"));
        assert!(super::should_use_workflow_root_span(false, "TemperAgent"));
        assert!(super::should_use_workflow_root_span(true, "Invoice"));
        assert!(!super::should_use_workflow_root_span(false, "Invoice"));
    }

    #[test]
    fn workflow_root_span_closes_only_on_root_terminal_status() {
        let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer_provider.tracer("temper-server-workflow-root-close-test")),
        );
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let registry = super::WorkflowSpanRegistry::default();
        registry.parent_context("default", "Session", "ss-1", "Session:ss-1", Some("ss-1"));

        assert!(!registry.finish_if_terminal("Session:ss-1", "Session", "ss-1", "Running"));
        assert!(!registry.finish_if_terminal("Session:ss-1", "Child", "child-1", "Completed"));
        assert!(registry.finish_if_terminal("Session:ss-1", "Session", "ss-1", "Completed"));
        assert!(!registry.finish_if_terminal("Session:ss-1", "Session", "ss-1", "Completed"));
    }

    #[test]
    fn workflow_root_drain_grace_is_explicit() {
        assert_eq!(super::workflow_root_drain_grace_ms(), 2_000);
    }

    #[test]
    fn workflow_root_drain_attribution_only_marks_root_terminal_status() {
        let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer_provider.tracer("temper-server-workflow-root-drain-test")),
        );
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let registry = super::WorkflowSpanRegistry::default();
        registry.parent_context("default", "Session", "ss-1", "Session:ss-1", Some("ss-1"));

        assert!(
            registry
                .mark_terminal_drain_scheduled("Session:ss-1", "Child", "child-1", "Completed")
                .is_none()
        );
        assert!(
            registry
                .mark_terminal_drain_scheduled("Session:ss-1", "Session", "ss-1", "Completed")
                .is_some()
        );
    }

    #[test]
    fn workflow_root_span_is_not_retained_without_otel_context() {
        let registry = super::WorkflowSpanRegistry::default();

        let parent =
            registry.parent_context("default", "Session", "ss-1", "Session:ss-1", Some("ss-1"));

        assert!(parent.is_none());
        assert!(registry.spans.lock().expect("spans lock").is_empty());
    }
}
