//! Shared HTTP request context types.
//!
//! Canonical home for request-scoped identity and session types extracted
//! from HTTP headers. These types are used across OData dispatch, authz,
//! observability, and reaction modules.

use axum::http::HeaderMap;

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
}

impl AgentContext {
    /// Create a system-level agent context for internal operations.
    ///
    /// Marks the provenance as `"system"` so that trajectories and events
    /// attribute the action to the platform itself rather than silently
    /// dropping identity via `Default`.
    pub fn system() -> Self {
        Self {
            agent_id: Some("system".to_string()),
            session_id: None,
            agent_type: None,
            intent: None,
            trace_id: None,
            parent_span_id: None,
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

    AgentContext {
        agent_id: None,
        session_id,
        agent_type: None,
        intent,
        trace_id,
        parent_span_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn extract_agent_context_session_and_intent() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "sess-abc".parse().unwrap());
        headers.insert("x-intent", "approve the invoice".parse().unwrap());
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
        headers.insert("x-temper-principal-id", "cc-a1b2c3".parse().unwrap());
        headers.insert("x-temper-agent-type", "claude-code".parse().unwrap());
        headers.insert("x-session-id", "sess-abc".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        // Identity headers are ignored — only credential resolution sets these.
        assert!(ctx.agent_id.is_none());
        assert!(ctx.agent_type.is_none());
        assert_eq!(ctx.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn extract_agent_context_ignores_empty_x_intent() {
        let mut headers = HeaderMap::new();
        headers.insert("x-intent", "".parse().unwrap());
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
        headers.insert("x-session-id", "".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert!(ctx.session_id.is_none());
    }
}
