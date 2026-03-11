use anyhow::Result;
use sha2::{Digest, Sha256};

/// Derive a deterministic agent ID from hostname and session ID.
///
/// Format: `cc-{sha256(hostname:session_id)[:12]}` — unique per machine per
/// session, stable across restarts of the same session.
fn derive_agent_id(session_id: &str) -> String {
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string()); // determinism-ok: startup config
    let mut hasher = Sha256::new();
    hasher.update(format!("{host}:{session_id}"));
    let hash = hex::encode(hasher.finalize());
    format!("cc-{}", &hash[..12])
}

pub async fn run(port: Option<u16>, url: Option<String>, agent_id: Option<String>) -> Result<()> {
    // Auto-derive identity from environment
    let session_id = std::env::var("CLAUDE_SESSION_ID").ok(); // determinism-ok: startup config
    let agent_type = Some("claude-code".to_string());

    let agent_id = if let Some(ref id) = agent_id {
        eprintln!(
            "temper-mcp: --agent-id is deprecated; identity is now auto-derived. \
             Using override: {id}"
        );
        agent_id
    } else {
        session_id
            .as_deref()
            .map(derive_agent_id)
            .or_else(|| Some("mcp-agent".to_string()))
    };

    temper_mcp::run_stdio_server(temper_mcp::McpConfig {
        temper_port: port,
        temper_url: url,
        agent_id,
        agent_type,
        session_id,
        api_key: std::env::var("TEMPER_API_KEY").ok(), // determinism-ok: startup config
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_agent_id_deterministic() {
        let a = derive_agent_id("sess-abc");
        let b = derive_agent_id("sess-abc");
        assert_eq!(a, b, "same input must produce same ID");
        assert!(a.starts_with("cc-"), "ID must start with cc- prefix");
        assert_eq!(a.len(), 15, "cc- (3) + 12 hex chars = 15");
    }

    #[test]
    fn test_derive_agent_id_different_sessions() {
        let a = derive_agent_id("sess-abc");
        let b = derive_agent_id("sess-xyz");
        assert_ne!(a, b, "different sessions must produce different IDs");
    }
}
