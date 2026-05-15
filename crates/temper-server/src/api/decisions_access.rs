//! Decision read access helpers.

use axum::http::HeaderMap;
use axum::response::Response;
use temper_authz::PrincipalKind;

use super::require_policy_auth;
use crate::authz::security_context_from_headers;
use crate::state::{PendingDecision, ServerState};

pub(crate) enum DecisionListAccess {
    Full,
    Owned {
        agent_id: String,
        session_id: Option<String>,
    },
}

impl DecisionListAccess {
    pub(crate) fn filter(&self, data_strings: Vec<String>) -> Vec<String> {
        match self {
            Self::Full => data_strings,
            Self::Owned {
                agent_id,
                session_id,
            } => data_strings
                .into_iter()
                .filter(|data| {
                    serde_json::from_str::<PendingDecision>(data)
                        .map(|decision| {
                            let same_session = session_id
                                .as_deref()
                                .is_some_and(|id| decision.session_id.as_deref() == Some(id));
                            decision.agent_id == *agent_id || same_session
                        })
                        .unwrap_or(false)
                })
                .collect(),
        }
    }
}

pub(crate) async fn decision_list_access(
    state: &ServerState,
    headers: &HeaderMap,
    tenant: &str,
) -> Result<DecisionListAccess, Response> {
    let security_ctx = security_context_from_headers(headers, None, None, None);
    if matches!(security_ctx.principal.kind, PrincipalKind::Admin) {
        return Ok(DecisionListAccess::Full);
    }

    if matches!(security_ctx.principal.kind, PrincipalKind::Agent) {
        return Ok(DecisionListAccess::Owned {
            agent_id: security_ctx.principal.id,
            session_id: security_ctx
                .context_attrs
                .get("sessionId")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned),
        });
    }

    match require_policy_auth(state, headers, tenant).await {
        Some(response) => Err(response),
        None => Ok(DecisionListAccess::Full),
    }
}
