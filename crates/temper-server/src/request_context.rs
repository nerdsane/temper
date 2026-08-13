//! Shared HTTP request context types.
//!
//! Canonical home for request-scoped identity and session types extracted
//! from HTTP headers. These types are used across OData dispatch, authz,
//! observability, and reaction modules.

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use temper_authz::SecurityContext;
use tracing_opentelemetry::OpenTelemetrySpanExt;

mod observation_metadata;

/// Agent identity context extracted from HTTP headers and credential resolution.
///
/// Threads identity through the dispatch chain for attribution in
/// trajectories, events, and WASM invocations.
///
/// Identity fields (`agent_id`, `agent_type`) are populated from the
/// credential-resolved `ResolvedIdentity` (ADR-0033), NOT from self-declared
/// headers. Only observability headers are extracted from HTTP:
/// - `X-Session-Id` / `X-Temper-Observe-Session-Id` — session grouping
/// - `X-Intent` / `X-Temper-Observe-Intent` — caller-supplied intent
/// - `X-Temper-Observe-Metadata` or `X-Temper-Observe-Meta-*` — generic,
///   namespaced observability metadata supplied by clients
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    /// Full Cedar security context when known at the request boundary.
    ///
    /// External HTTP entrypoints populate this after credential resolution so
    /// downstream trigger dispatch can inherit the exact principal rather than
    /// approximating it from partial agent metadata.
    pub security_ctx: Option<SecurityContext>,
    /// Optional agent identifier. Populated from `ResolvedIdentity` when
    /// credential resolution succeeds, or from internal system context.
    pub agent_id: Option<String>,
    /// Optional session identifier (from `X-Session-Id` header).
    pub session_id: Option<String>,
    /// Optional agent type classification. Populated from `ResolvedIdentity`
    /// when credential resolution succeeds.
    pub agent_type: Option<String>,
    /// Optional intent description (from `X-Intent` header).
    ///
    /// Captured on failed requests so the Evolution Engine can surface
    /// exactly what the agent was trying to accomplish.
    pub intent: Option<String>,
    /// W3C trace ID extracted from the `traceparent` header.
    /// Propagated through WASM HTTP calls to unify agent lifecycle traces.
    pub trace_id: Option<String>,
    /// Parent span ID from the `traceparent` header.
    pub parent_span_id: Option<String>,
    /// Root entity type for the logical workflow trace.
    ///
    /// This is observability metadata carried in dispatch context. It is not
    /// persisted as entity business state.
    pub workflow_root_entity_type: Option<String>,
    /// Root entity ID for the logical workflow trace.
    pub workflow_root_entity_id: Option<String>,
    /// Stable workflow run identifier used as a queryable APM/log attribute.
    pub workflow_run_id: Option<String>,
    /// ADR-0048 sub-decision 5: idempotency key extracted from the
    /// `Idempotency-Key` header. Threaded into `EntityMsg::Action` so the
    /// actor can dedupe duplicate asks produced by dispatch-layer retries.
    pub idempotency_key: Option<String>,
    /// Host-only optimistic concurrency precondition checked by the entity actor.
    pub expected_entity_sequence: Option<u64>,
    /// Generic, client-supplied observability metadata.
    ///
    /// Producers should namespace their keys, for example
    /// `workflow.run_id`, `producer.work_item_id`, or `support.ticket_id`.
    /// Temper core treats these keys as opaque correlation metadata.
    pub observation_metadata: BTreeMap<String, String>,
}

impl AgentContext {
    /// Create a system-level agent context for internal operations.
    ///
    /// Marks the provenance as `"system"` so that trajectories and events
    /// attribute the action to the platform itself rather than silently
    /// dropping identity via `Default`.
    pub fn system() -> Self {
        Self {
            security_ctx: Some(SecurityContext::system()),
            agent_id: Some("system".to_string()),
            session_id: None,
            agent_type: None,
            intent: None,
            trace_id: None,
            parent_span_id: None,
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            idempotency_key: None,
            expected_entity_sequence: None,
            observation_metadata: BTreeMap::new(),
        }
    }

    /// ADR-0046: Create an `AgentContext` for a named platform service.
    ///
    /// Used in place of [`AgentContext::system`] to give callers an
    /// explicit, auditable identity. The service name populates
    /// `agent_type` so Cedar policies can match on
    /// `principal.agent_type == "<service>"` — narrower than the
    /// broad-permit `system-platform` policy.
    ///
    /// Recommended service names are tracked in
    /// `docs/adrs/0046-unified-action-triggers.md`.
    pub fn for_service(service_name: &str) -> Self {
        let service_id = format!("service:{service_name}");
        let mut security_ctx = SecurityContext::from_headers(&[]).with_agent_context(
            Some(&service_id),
            None,
            Some(service_name),
        );
        security_ctx.principal.role = Some("service".to_string());

        Self {
            security_ctx: Some(security_ctx),
            agent_id: Some(service_id),
            session_id: None,
            agent_type: Some(service_name.to_string()),
            intent: None,
            trace_id: None,
            parent_span_id: None,
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            idempotency_key: None,
            expected_entity_sequence: None,
            observation_metadata: BTreeMap::new(),
        }
    }

    /// Create a system-level context for a named internal transport or adapter.
    pub fn system_with_agent_id(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: Some(agent_id.into()),
            agent_type: Some("system".to_string()),
            ..Self::default()
        }
    }

    /// Create a service identity while preserving caller observability context.
    ///
    /// Service callbacks often need a narrower principal than the invoking
    /// agent, but they still belong to the same logical workflow trace.
    pub fn for_service_inheriting(service_name: &str, parent: &AgentContext) -> Self {
        Self::for_service(service_name).inherit_observability_from(parent)
    }

    /// Copy non-authority observability fields from another dispatch context.
    pub fn inherit_observability_from(mut self, parent: &AgentContext) -> Self {
        self.session_id = parent.session_id.clone();
        self.intent = parent.intent.clone();
        self.trace_id = parent.trace_id.clone();
        self.parent_span_id = parent.parent_span_id.clone();
        self.workflow_root_entity_type = parent.workflow_root_entity_type.clone();
        self.workflow_root_entity_id = parent.workflow_root_entity_id.clone();
        self.workflow_run_id = parent.workflow_run_id.clone();
        self.observation_metadata = parent.observation_metadata.clone();
        self
    }

    /// Ensure this context has a workflow root and current-span trace ids.
    pub fn for_dispatch_root(&self, entity_type: &str, entity_id: &str) -> Self {
        let mut next = self.clone();
        if next.workflow_root_entity_type.is_none() {
            next.workflow_root_entity_type = Some(entity_type.to_string());
        }
        if next.workflow_root_entity_id.is_none() {
            next.workflow_root_entity_id = Some(entity_id.to_string());
        }
        if next.workflow_run_id.is_none() {
            let root_type = next
                .workflow_root_entity_type
                .as_deref()
                .unwrap_or(entity_type);
            let root_id = next.workflow_root_entity_id.as_deref().unwrap_or(entity_id);
            next.workflow_run_id = Some(format!("{root_type}:{root_id}"));
        }
        next.with_current_span_trace_context()
    }

    /// Fill missing W3C trace IDs from the currently active OpenTelemetry span.
    pub fn with_current_span_trace_context(mut self) -> Self {
        if let Some((trace_id, span_id)) = current_span_trace_context_ids() {
            self.trace_id = Some(trace_id);
            self.parent_span_id = Some(span_id);
        }
        self
    }

    /// Serialize observation metadata for log fields.
    pub fn observation_metadata_json(&self) -> Option<String> {
        if self.observation_metadata.is_empty() {
            return None;
        }
        serde_json::to_string(&self.observation_metadata).ok()
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Extract observability context from request headers.
///
/// Reads generic session, intent, and observation metadata headers for
/// observability purposes.
/// Identity fields (`agent_id`, `agent_type`) are NOT extracted from
/// self-declared headers — they come from credential resolution (ADR-0033)
/// or are set to `None` for anonymous/operator access.
pub(crate) fn extract_agent_context(headers: &HeaderMap) -> AgentContext {
    let session_id = header_string(headers, "x-temper-observe-session-id")
        .or_else(|| header_string(headers, "x-session-id"));
    let intent = header_string(headers, "x-temper-observe-intent")
        .or_else(|| header_string(headers, "x-intent"));
    // Extract W3C traceparent: "00-{trace_id}-{parent_span_id}-{flags}"
    let (trace_id, parent_span_id) = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .and_then(|tp| {
            let parts: Vec<&str> = tp.split('-').collect();
            if parts.len() >= 4 && parts[1].len() == 32 && parts[2].len() == 16 {
                Some((parts[1].to_string(), parts[2].to_string()))
            } else {
                None
            }
        })
        .map(|(t, s)| (Some(t), Some(s)))
        .unwrap_or((None, None));

    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let workflow_root_entity_type = header_string(headers, "x-temper-workflow-root-entity-type");
    let workflow_root_entity_id = header_string(headers, "x-temper-workflow-root-entity-id");
    let workflow_run_id = header_string(headers, "x-temper-workflow-run-id");

    AgentContext {
        security_ctx: None,
        agent_id: None,
        session_id,
        agent_type: None,
        intent,
        trace_id,
        parent_span_id,
        workflow_root_entity_type,
        workflow_root_entity_id,
        workflow_run_id,
        idempotency_key,
        expected_entity_sequence: None,
        observation_metadata: observation_metadata::extract(headers),
    }
}

pub(crate) fn current_span_trace_context_ids() -> Option<(String, String)> {
    let span_context = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .clone();
    if !span_context.is_valid() {
        return None;
    }
    Some((
        span_context.trace_id().to_string(),
        span_context.span_id().to_string(),
    ))
}

pub(crate) fn remote_parent_context(agent_ctx: &AgentContext) -> Option<opentelemetry::Context> {
    let trace_id = TraceId::from_hex(agent_ctx.trace_id.as_deref()?).ok()?;
    let span_id = SpanId::from_hex(agent_ctx.parent_span_id.as_deref()?).ok()?;
    let span_context = SpanContext::new(
        trace_id,
        span_id,
        TraceFlags::SAMPLED,
        true,
        TraceState::default(),
    );
    Some(opentelemetry::Context::new().with_remote_span_context(span_context))
}

#[cfg(test)]
mod tests {
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
        // Identity fields are never extracted from headers (ADR-0033).
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
        // Identity headers are ignored — only credential resolution sets these.
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
}
