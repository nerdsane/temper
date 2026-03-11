//! Shared HTTP request context types.
//!
//! Canonical home for request-scoped identity and session types extracted
//! from HTTP headers. These types are used across OData dispatch, authz,
//! observability, and reaction modules.

use axum::http::HeaderMap;

/// Agent identity context extracted from HTTP headers.
///
/// Threads identity through the dispatch chain for attribution in
/// trajectories, events, and WASM invocations.
///
/// All identity headers use the `X-Temper-*` namespace:
/// - `X-Temper-Principal-Id` — unique agent instance ID
/// - `X-Temper-Agent-Type` — agent software classification (e.g. `claude-code`)
/// - `X-Session-Id` — session grouping
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    /// Optional agent identifier (from `X-Temper-Principal-Id` header).
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
/// Reads `X-Temper-Principal-Id`, `X-Session-Id`, and `X-Temper-Agent-Type`.
/// Returns defaults (all `None`) if headers are absent or empty.
pub(crate) fn extract_agent_context(headers: &HeaderMap) -> AgentContext {
    let agent_id = headers
        .get("x-temper-principal-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && *s != "anonymous")
        .map(String::from);
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
    fn extract_agent_context_principal_and_session() {
        let mut headers = HeaderMap::new();
        headers.insert("x-temper-principal-id", "cc-a1b2c3".parse().unwrap());
        headers.insert("x-session-id", "sess-abc".parse().unwrap());
        headers.insert("x-temper-agent-type", "claude-code".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert_eq!(ctx.agent_id.as_deref(), Some("cc-a1b2c3"));
        assert_eq!(ctx.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(ctx.agent_type.as_deref(), Some("claude-code"));
    }

    #[test]
    fn extract_agent_context_missing_headers() {
        let headers = HeaderMap::new();
        let ctx = extract_agent_context(&headers);
        assert!(ctx.agent_id.is_none());
        assert!(ctx.session_id.is_none());
        assert!(ctx.agent_type.is_none());
    }

    #[test]
    fn extract_agent_context_empty_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-temper-principal-id", "".parse().unwrap());
        headers.insert("x-session-id", "".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert!(ctx.agent_id.is_none());
        assert!(ctx.session_id.is_none());
    }

    #[test]
    fn extract_agent_context_ignores_anonymous_principal() {
        let mut headers = HeaderMap::new();
        headers.insert("x-temper-principal-id", "anonymous".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert!(ctx.agent_id.is_none());
    }
}
