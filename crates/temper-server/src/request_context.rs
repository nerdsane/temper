//! Shared request-scoped identity and session types used by HTTP, OData,
//! authorization, observability, and reaction dispatch.

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use temper_authz::SecurityContext;
use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
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
    /// Host-only optimistic concurrency precondition checked by the actor.
    pub expected_entity_sequence: Option<u64>,
    /// Host-resolved immutable task-scoped schema identity.
    ///
    /// HTTP adapters validate explicit digests. Scope-only entity requests use
    /// the entity's durable pin when present and the active pointer for creation.
    pub schema_pin: Option<SchemaExecutionPin>,
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
            schema_pin: None,
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
        let mut security_ctx = SecurityContext::anonymous().with_agent_context(
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
            schema_pin: None,
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
        self.schema_pin = parent.schema_pin.clone();
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

/// Extract the caller-supplied session id from observability headers.
///
/// Accepts `X-Temper-Observe-Session-Id` and the shorter `X-Session-Id` alias.
/// Single source of truth so every entrypoint honours both spellings.
pub fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    header_string(headers, "x-temper-observe-session-id")
        .or_else(|| header_string(headers, "x-session-id"))
}

/// Extract the caller-supplied intent from observability headers.
///
/// Accepts `X-Temper-Observe-Intent` and the shorter `X-Intent` alias.
pub fn intent_from_headers(headers: &HeaderMap) -> Option<String> {
    header_string(headers, "x-temper-observe-intent").or_else(|| header_string(headers, "x-intent"))
}

/// Extract observability context from request headers.
///
/// Reads generic session, intent, and observation metadata headers for
/// observability purposes.
/// Identity fields (`agent_id`, `agent_type`) are NOT extracted from
/// self-declared headers — they come from credential resolution (ADR-0033)
/// or are set to `None` for anonymous/operator access.
pub(crate) fn extract_agent_context(headers: &HeaderMap) -> AgentContext {
    let session_id = session_id_from_headers(headers);
    let intent = intent_from_headers(headers);
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
        schema_pin: None,
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
#[path = "request_context_test.rs"]
mod legacy_tests;

#[cfg(test)]
#[path = "request_context/mod_test.rs"]
mod tests;
