//! Decision read access helpers.

use axum::response::Response;
use temper_authz::{AuthenticatedRequestContext, PrincipalKind};

use super::require_policy_auth;
use crate::state::{PendingDecision, ServerState};

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
