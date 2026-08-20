use super::*;
use axum::http::{HeaderMap, HeaderValue};
use opentelemetry::trace::TraceContextExt;

#[test]
fn extract_agent_context_session_intent_and_metadata() {
    let mut headers = HeaderMap::new();
    headers.insert("x-session-id", HeaderValue::from_static("sess-abc"));
    headers.insert("x-intent", HeaderValue::from_static("approve the invoice"));
    headers.insert(
        "x-temper-observe-metadata",
        HeaderValue::from_static(
            r#"{"workflow.run_id":"seed-usage:agent-answers-seed:sim-user-1","producer.user_id":"sim-user-1"}"#,
        ),
    );
    headers.insert(
        "x-temper-observe-meta-producer.work_item_id",
        HeaderValue::from_static("wi-123"),
    );
    let ctx = extract_agent_context(&headers);
    assert_eq!(ctx.session_id.as_deref(), Some("sess-abc"));
    assert_eq!(ctx.intent.as_deref(), Some("approve the invoice"));
    assert_eq!(
        ctx.observation_metadata
            .get("workflow.run_id")
            .map(String::as_str),
        Some("seed-usage:agent-answers-seed:sim-user-1")
    );
    assert_eq!(
        ctx.observation_metadata
            .get("producer.user_id")
            .map(String::as_str),
        Some("sim-user-1")
    );
    assert_eq!(
        ctx.observation_metadata
            .get("producer.work_item_id")
            .map(String::as_str),
        Some("wi-123")
    );
    assert!(ctx.agent_id.is_none());
    assert!(ctx.agent_type.is_none());
}

#[test]
fn extract_agent_context_workflow_observability_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-temper-workflow-root-entity-type",
        HeaderValue::from_static("CurationQuery"),
    );
    headers.insert(
        "x-temper-workflow-root-entity-id",
        HeaderValue::from_static("cq-1"),
    );
    headers.insert(
        "x-temper-workflow-run-id",
        HeaderValue::from_static("CurationQuery:cq-1"),
    );

    let ctx = extract_agent_context(&headers);
    assert_eq!(
        ctx.workflow_root_entity_type.as_deref(),
        Some("CurationQuery")
    );
    assert_eq!(ctx.workflow_root_entity_id.as_deref(), Some("cq-1"));
    assert_eq!(ctx.workflow_run_id.as_deref(), Some("CurationQuery:cq-1"));
}

#[test]
fn extract_agent_context_ignores_identity_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-temper-principal-id",
        HeaderValue::from_static("cc-a1b2c3"),
    );
    headers.insert(
        "x-temper-agent-type",
        HeaderValue::from_static("claude-code"),
    );
    headers.insert("x-session-id", HeaderValue::from_static("sess-abc"));
    let ctx = extract_agent_context(&headers);
    assert!(ctx.agent_id.is_none());
    assert!(ctx.agent_type.is_none());
    assert_eq!(ctx.session_id.as_deref(), Some("sess-abc"));
}

#[test]
fn extract_agent_context_ignores_empty_x_intent() {
    let mut headers = HeaderMap::new();
    headers.insert("x-intent", HeaderValue::from_static(""));
    let ctx = extract_agent_context(&headers);
    assert!(ctx.intent.is_none());
}

#[test]
fn extract_agent_context_missing_headers() {
    let headers = HeaderMap::new();
    let ctx = extract_agent_context(&headers);
    assert!(ctx.agent_id.is_none());
    assert!(ctx.session_id.is_none());
    assert!(ctx.agent_type.is_none());
    assert!(ctx.intent.is_none());
    assert!(ctx.observation_metadata.is_empty());
}

#[test]
fn extract_agent_context_empty_session() {
    let mut headers = HeaderMap::new();
    headers.insert("x-session-id", HeaderValue::from_static(""));
    let ctx = extract_agent_context(&headers);
    assert!(ctx.session_id.is_none());
}

#[test]
fn remote_parent_context_builds_remote_span_context() {
    let agent_ctx = AgentContext {
        trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".to_string()),
        parent_span_id: Some("00f067aa0ba902b7".to_string()),
        ..AgentContext::default()
    };

    let remote = remote_parent_context(&agent_ctx).expect("valid remote trace context");
    let span_context = remote.span().span_context().clone();

    assert!(span_context.is_remote());
    assert_eq!(
        span_context.trace_id().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_eq!(span_context.span_id().to_string(), "00f067aa0ba902b7");
}

#[test]
fn service_context_inherits_workflow_trace_context() {
    let parent = AgentContext {
        session_id: Some("ss-1".to_string()),
        intent: Some("run workflow".to_string()),
        trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".to_string()),
        parent_span_id: Some("00f067aa0ba902b7".to_string()),
        workflow_root_entity_type: Some("CurationJob".to_string()),
        workflow_root_entity_id: Some("job-1".to_string()),
        workflow_run_id: Some("wf-job-1".to_string()),
        idempotency_key: Some("parent-key".to_string()),
        ..AgentContext::default()
    };

    let service = AgentContext::for_service_inheriting("wasm-runtime", &parent);

    assert_eq!(service.agent_type.as_deref(), Some("wasm-runtime"));
    assert_eq!(service.session_id.as_deref(), Some("ss-1"));
    assert_eq!(service.intent.as_deref(), Some("run workflow"));
    assert_eq!(service.trace_id.as_deref(), parent.trace_id.as_deref());
    assert_eq!(
        service.parent_span_id.as_deref(),
        parent.parent_span_id.as_deref()
    );
    assert_eq!(
        service.workflow_root_entity_type.as_deref(),
        Some("CurationJob")
    );
    assert_eq!(service.workflow_root_entity_id.as_deref(), Some("job-1"));
    assert_eq!(service.workflow_run_id.as_deref(), Some("wf-job-1"));
    assert!(service.idempotency_key.is_none());
}

#[test]
fn dispatch_context_sets_root_workflow_and_current_span_parent() {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::prelude::*;

    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(tracer_provider.tracer("temper-server-request-context-test")),
    );
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let span = tracing::info_span!("dispatch.Workflow.Start");

    let enriched =
        span.in_scope(|| AgentContext::default().for_dispatch_root("CurationJob", "job-1"));

    assert_eq!(
        enriched.workflow_root_entity_type.as_deref(),
        Some("CurationJob")
    );
    assert_eq!(enriched.workflow_root_entity_id.as_deref(), Some("job-1"));
    assert_eq!(
        enriched.workflow_run_id.as_deref(),
        Some("CurationJob:job-1")
    );
    assert!(
        enriched
            .trace_id
            .as_deref()
            .is_some_and(|id| id.len() == 32)
    );
    assert!(
        enriched
            .parent_span_id
            .as_deref()
            .is_some_and(|id| id.len() == 16)
    );
}
