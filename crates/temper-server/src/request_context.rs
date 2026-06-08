//! Shared HTTP request context types.
//!
//! Canonical home for request-scoped identity and session types extracted
//! from HTTP headers. These types are used across OData dispatch, authz,
//! observability, and reaction modules.

use axum::http::HeaderMap;
use std::collections::BTreeMap;

const OBSERVATION_METADATA_HEADER: &str = "x-temper-observe-metadata";
const OBSERVATION_METADATA_HEADER_PREFIX: &str = "x-temper-observe-meta-";
const MAX_OBSERVATION_METADATA_KEYS: usize = 32;
const MAX_OBSERVATION_METADATA_KEY_BYTES: usize = 96;
const MAX_OBSERVATION_METADATA_VALUE_BYTES: usize = 1024;

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
            agent_id: Some("system".to_string()),
            session_id: None,
            agent_type: None,
            intent: None,
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

fn metadata_value_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn valid_metadata_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_OBSERVATION_METADATA_KEY_BYTES
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn truncate_metadata_value(value: &str) -> String {
    if value.len() <= MAX_OBSERVATION_METADATA_VALUE_BYTES {
        return value.to_string();
    }
    let mut end = MAX_OBSERVATION_METADATA_VALUE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn insert_observation_metadata(metadata: &mut BTreeMap<String, String>, key: &str, value: String) {
    if metadata.len() >= MAX_OBSERVATION_METADATA_KEYS || !valid_metadata_key(key) {
        return;
    }
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    metadata.insert(key.to_string(), truncate_metadata_value(value));
}

fn extract_observation_metadata(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();

    if let Some(raw) = header_string(headers, OBSERVATION_METADATA_HEADER)
        && raw.len() <= 8192
        && let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&raw)
    {
        for (key, value) in map {
            if let Some(value) = metadata_value_string(&value) {
                insert_observation_metadata(&mut metadata, &key, value);
            }
        }
    }

    for (name, value) in headers {
        let name = name.as_str();
        let Some(key) = name.strip_prefix(OBSERVATION_METADATA_HEADER_PREFIX) else {
            continue;
        };
        let Some(value) = value
            .to_str()
            .ok()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        insert_observation_metadata(&mut metadata, key, value);
    }

    metadata
}

/// Extract observability context from request headers.
///
/// Reads generic session, intent, and observation metadata headers for
/// observability purposes.
/// Identity fields (`agent_id`, `agent_type`) are NOT extracted from
/// self-declared headers — they come from credential resolution (ADR-0033)
/// or are set to `None` for anonymous/operator access.
pub(crate) fn extract_agent_context(headers: &HeaderMap) -> AgentContext {
    AgentContext {
        agent_id: None,
        session_id: header_string(headers, "x-temper-observe-session-id")
            .or_else(|| header_string(headers, "x-session-id")),
        agent_type: None,
        intent: header_string(headers, "x-temper-observe-intent")
            .or_else(|| header_string(headers, "x-intent")),
        observation_metadata: extract_observation_metadata(headers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn extract_agent_context_session_intent_and_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "sess-abc".parse().unwrap());
        headers.insert("x-intent", "approve the invoice".parse().unwrap());
        headers.insert(
            "x-temper-observe-metadata",
            r#"{"workflow.run_id":"seed-usage:agent-answers-seed:sim-user-1","producer.user_id":"sim-user-1"}"#
                .parse()
                .unwrap(),
        );
        headers.insert(
            "x-temper-observe-meta-producer.work_item_id",
            "wi-123".parse().unwrap(),
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
        assert!(ctx.observation_metadata.is_empty());
    }

    #[test]
    fn extract_agent_context_empty_session() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "".parse().unwrap());
        let ctx = extract_agent_context(&headers);
        assert!(ctx.session_id.is_none());
    }
}
