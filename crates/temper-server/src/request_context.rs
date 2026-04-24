//! Shared HTTP request context types.
//!
//! Canonical home for request-scoped identity and session types extracted
//! from HTTP headers. These types are used across OData dispatch, authz,
//! observability, and reaction modules.

use axum::http::HeaderMap;
use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use temper_authz::SecurityContext;

/// Agent identity context extracted from HTTP headers and credential resolution.
///
/// Threads identity through the dispatch chain for attribution in
/// trajectories, events, and WASM invocations.
///
/// Identity fields (`agent_id`, `agent_type`) are populated from the
/// credential-resolved `ResolvedIdentity` (ADR-0033), NOT from self-declared
/// headers. Only observability headers are extracted from HTTP:
/// - `X-Session-Id` — session grouping
/// - `X-Intent` — caller-supplied description of what they were trying to do
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
    /// ADR-0048 sub-decision 5: idempotency key extracted from the
    /// `Idempotency-Key` header. Threaded into `EntityMsg::Action` so the
    /// actor can dedupe duplicate asks produced by dispatch-layer retries.
    pub idempotency_key: Option<String>,
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
            idempotency_key: None,
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
    /// Recommended service names (see `docs/adrs/0046-unified-action-triggers.md`
    /// migration audit):
    /// - `platform-bootstrap` — tenant/app install, credential rotation
    /// - `governance-service` — ADR-0014 governance callbacks
    /// - `evolution-engine` — Observation/Problem/Analysis materialization
    /// - `timeout-scheduler` — state-timeout firings (ADR-0049)
    /// - `platform-dispatch` — dispatch fallback paths
    /// - `wasm-runtime` — WASM invocation artifact creation
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
            idempotency_key: None,
        }
    }
}

/// Extract observability context from request headers.
///
/// Reads `X-Session-Id` and `X-Intent` for observability purposes.
/// Identity fields (`agent_id`, `agent_type`) are NOT extracted from
/// self-declared headers — they come from credential resolution (ADR-0033)
/// or are set to `None` for anonymous/operator access.
pub(crate) fn extract_agent_context(headers: &HeaderMap) -> AgentContext {
    let session_id = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let intent = headers
        .get("x-intent")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from);
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

    AgentContext {
        security_ctx: None,
        agent_id: None,
        session_id,
        agent_type: None,
        intent,
        trace_id,
        parent_span_id,
        idempotency_key,
    }
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
    fn extract_agent_context_session_and_intent() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", HeaderValue::from_static("sess-abc"));
        headers.insert("x-intent", HeaderValue::from_static("approve the invoice"));
        let ctx = extract_agent_context(&headers);
        assert_eq!(ctx.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(ctx.intent.as_deref(), Some("approve the invoice"));
        // Identity fields are never extracted from headers (ADR-0033).
        assert!(ctx.agent_id.is_none());
        assert!(ctx.agent_type.is_none());
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
}
