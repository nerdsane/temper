//! Decision read access helpers.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind};

use super::require_policy_auth;
use crate::state::{PendingDecision, ServerState};

/// True when the caller is the principal who was denied.
pub(crate) fn is_self_resolution(principal_id: &str, denied_agent_id: &str) -> bool {
    principal_id == denied_agent_id
}

/// Forbid the denied principal from approving or denying their own decision.
///
/// Independent of Cedar (ADR-0172). A caller with `manage_policies` still
/// cannot resolve a decision whose `agent_id` is their own principal id.
pub(crate) fn reject_self_resolution(
    principal_id: &str,
    decision: &PendingDecision,
) -> Option<Response> {
    if !is_self_resolution(principal_id, &decision.agent_id) {
        return None;
    }
    tracing::warn!(
        decision_id = %decision.id,
        agent_id = %decision.agent_id,
        "denied principal cannot approve or deny their own decision"
    );
    Some(
        (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "AuthorizationDenied",
                    "message": "The denied principal cannot approve or deny this decision",
                }
            })),
        )
            .into_response(),
    )
}

pub(crate) enum DecisionListAccess {
    Full,
    Owned { agent_id: String },
}

impl DecisionListAccess {
    pub(crate) fn filter(&self, data_strings: Vec<String>) -> Vec<String> {
        match self {
            Self::Full => data_strings,
            Self::Owned { agent_id } => data_strings
                .into_iter()
                .filter(|data| {
                    serde_json::from_str::<PendingDecision>(data)
                        .map(|decision| decision.agent_id == *agent_id)
                        .unwrap_or(false)
                })
                .collect(),
        }
    }
}

pub(crate) async fn decision_list_access(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
) -> Result<DecisionListAccess, Response> {
    let security_ctx = authenticated.security_context();
    if matches!(security_ctx.principal.kind, PrincipalKind::Agent) {
        return Ok(DecisionListAccess::Owned {
            agent_id: security_ctx.principal.id.clone(),
        });
    }

    match require_policy_auth(state, authenticated).await {
        Some(response) => Err(response),
        None => Ok(DecisionListAccess::Full),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_resolution_matches_denied_principal_only() {
        assert!(is_self_resolution("developer", "developer"));
        assert!(!is_self_resolution("operator", "developer"));
        assert!(!is_self_resolution("developer", "operator"));
    }

    #[test]
    fn reject_self_resolution_blocks_denied_principal() {
        let decision = PendingDecision::from_denial(
            "acme",
            "developer",
            "Assign",
            "Issue",
            "issue-1",
            serde_json::json!({"id": "issue-1"}),
            "denied",
            None,
        );
        assert!(reject_self_resolution("developer", &decision).is_some());
        assert!(reject_self_resolution("operator", &decision).is_none());
    }
}
