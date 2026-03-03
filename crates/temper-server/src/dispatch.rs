//! Compatibility facade for OData dispatch handlers.
//!
//! The implementation lives under [`crate::odata`]. This module keeps
//! existing imports stable while the OData stack is split into smaller files.

use axum::http::HeaderMap;

#[cfg(feature = "observe")]
pub(crate) use crate::odata::extract_tenant;
pub use crate::odata::handle_hints;
pub use crate::odata::handle_metadata;
pub use crate::odata::handle_odata_delete;
pub use crate::odata::handle_odata_get;
pub use crate::odata::handle_odata_patch;
pub use crate::odata::handle_odata_post;
pub use crate::odata::handle_odata_put;
pub use crate::odata::handle_service_document;

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
    AgentContext {
        agent_id,
        session_id,
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
