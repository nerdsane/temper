//! Identity resolver cache invalidation middleware.
//!
//! Keeps bearer-auth credential cache coherent by evicting entries after
//! successful `AgentCredential` lifecycle mutations.

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;

use crate::state::PlatformState;

/// Invalidate credential cache entries after successful credential mutations.
///
/// This middleware watches successful bound action calls on `AgentCredentials`
/// and evicts the corresponding key-hash entry from the shared resolver cache.
pub async fn invalidate_identity_cache_on_credential_mutation(
    State(state): State<PlatformState>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let response = next.run(req).await;

    if response.status().is_success()
        && method == Method::POST
        && let Some(key_hash) = extract_credential_key_hash(&path)
    {
        state.identity_resolver.invalidate_key_hash(key_hash);
    }

    response
}

fn extract_credential_key_hash(path: &str) -> Option<&str> {
    let prefix = "/tdata/AgentCredentials('";
    let rest = path.strip_prefix(prefix)?;
    let (key_hash, action) = rest.split_once("')/Temper.Agent.")?;

    if key_hash.is_empty() {
        return None;
    }

    match action {
        "Issue" | "Rotate" | "Revoke" => Some(key_hash),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::extract_credential_key_hash;

    #[test]
    fn extracts_key_hash_for_issue() {
        let path = "/tdata/AgentCredentials('abc123')/Temper.Agent.Issue";
        assert_eq!(extract_credential_key_hash(path), Some("abc123"));
    }

    #[test]
    fn extracts_key_hash_for_rotate_and_revoke() {
        let rotate = "/tdata/AgentCredentials('deadbeef')/Temper.Agent.Rotate";
        let revoke = "/tdata/AgentCredentials('deadbeef')/Temper.Agent.Revoke";
        assert_eq!(extract_credential_key_hash(rotate), Some("deadbeef"));
        assert_eq!(extract_credential_key_hash(revoke), Some("deadbeef"));
    }

    #[test]
    fn ignores_non_credential_paths() {
        assert!(extract_credential_key_hash("/tdata/Orders('o1')/Temper.Approve").is_none());
        assert!(
            extract_credential_key_hash("/tdata/AgentCredentials('abc')/Temper.Agent.Define")
                .is_none()
        );
    }
}
