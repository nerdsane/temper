//! Shared HTTP request context types.
//!
//! Canonical home for request-scoped identity and session types extracted
//! from HTTP headers. These types are used across OData dispatch, authz,
//! observability, and reaction modules.

use axum::http::HeaderMap;

/// Agent identity context extracted from HTTP headers.
///
/// Threads `X-Agent-Id` and `X-Session-Id` through the dispatch chain
/// for attribution in trajectories, events, and WASM invocations.
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    /// Optional agent identifier (from `X-Agent-Id` header).
    pub agent_id: Option<String>,
    /// Optional session identifier (from `X-Session-Id` header).
    pub session_id: Option<String>,
    /// Optional agent type classification (from `X-Temper-Agent-Type` header).
    pub agent_type: Option<String>,
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
        }
    }
}

/// Extract agent identity from request headers.
///
/// Reads `X-Agent-Id` and `X-Session-Id` headers. Returns defaults
/// (both `None`) if headers are absent or empty.
pub(crate) fn extract_agent_context(headers: &HeaderMap) -> AgentContext {
    let agent_id = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            // Fall back to X-Temper-Principal-Id for agents using the
            // Cedar security context headers instead of X-Agent-Id.
            headers
                .get("x-temper-principal-id")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty() && s != &"anonymous")
                .map(String::from)
        });
    let session_id = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let agent_type = headers
        .get("x-temper-agent-type")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from);
    AgentContext {
        agent_id,
        session_id,
        agent_type,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn extract_agent_context_both_present() {
        let mut headers = HeaderMap::new();
        headers.insert("x-agent-id", "agent-007".parse().unwrap());
        headers.insert("x-session-id", "sess-abc".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert_eq!(ctx.agent_id.as_deref(), Some("agent-007"));
        assert_eq!(ctx.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn extract_agent_context_missing_headers() {
        let headers = HeaderMap::new();
        let ctx = extract_agent_context(&headers);
        assert!(ctx.agent_id.is_none());
        assert!(ctx.session_id.is_none());
    }

    #[test]
    fn extract_agent_context_empty_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-agent-id", "".parse().unwrap());
        headers.insert("x-session-id", "".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert!(ctx.agent_id.is_none());
        assert!(ctx.session_id.is_none());
    }

    #[test]
    fn extract_agent_context_falls_back_to_temper_principal() {
        let mut headers = HeaderMap::new();
        headers.insert("x-temper-principal-id", "checkout-bot".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert_eq!(ctx.agent_id.as_deref(), Some("checkout-bot"));
    }

    #[test]
    fn extract_agent_context_prefers_x_agent_id() {
        let mut headers = HeaderMap::new();
        headers.insert("x-agent-id", "agent-007".parse().unwrap());
        headers.insert("x-temper-principal-id", "checkout-bot".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert_eq!(ctx.agent_id.as_deref(), Some("agent-007"));
    }

    #[test]
    fn extract_agent_context_ignores_anonymous_principal() {
        let mut headers = HeaderMap::new();
        headers.insert("x-temper-principal-id", "anonymous".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert!(ctx.agent_id.is_none());
    }
}
