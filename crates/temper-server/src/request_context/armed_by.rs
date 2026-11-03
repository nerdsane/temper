//! The identity a durable timer row keeps, so a timer re-armed after a restart
//! fires as whoever armed it, with what they could read when they set it.
//! State timer rows and scheduled-action rows keep it in the same form.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use temper_authz::SecurityContext;

use super::AgentContext;
use crate::idempotency::IdempotencyKey;

/// The identity part of an [`AgentContext`] that a timer row keeps. Runtime
/// budgets and the idempotency key are kept only for scheduled follow-ons.
/// State timers fire as new background actions.
#[derive(Serialize, Deserialize)]
struct ArmedBy {
    security_ctx: Option<SecurityContext>,
    agent_id: Option<String>,
    session_id: Option<String>,
    agent_type: Option<String>,
    intent: Option<String>,
    trace_id: Option<String>,
    parent_span_id: Option<String>,
    workflow_root_entity_type: Option<String>,
    workflow_root_entity_id: Option<String>,
    workflow_run_id: Option<String>,
    #[serde(default)]
    observation_metadata: BTreeMap<String, String>,
    /// Absent from rows written before timers kept it, and ignored by a
    /// binary that predates it: such a timer fires as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_when_set: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scheduled_follow_on: Option<ScheduledFollowOn>,
    /// Only call deadlines carry an origin process; old rows have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call_process_id: Option<uuid::Uuid>,
}

/// The causal context of a scheduled action, already separated from its parent.
#[derive(Serialize, Deserialize)]
struct ScheduledFollowOn {
    callback_hops: u32,
    idempotency_key: Option<String>,
}

/// `agent`'s identity as a row keeps it (JSON).
pub(crate) fn encode_armed_by(agent: &AgentContext) -> Result<String, serde_json::Error> {
    serde_json::to_string(&armed_by(agent))
}

/// A scheduled follow-on's identity, lineage and derived key as its row keeps them.
pub(crate) fn encode_scheduled_armed_by(agent: &AgentContext) -> Result<String, serde_json::Error> {
    encode_follow_on(agent, None)
}

/// A call deadline's identity and the process that owns its live call.
pub(crate) fn encode_call_armed_by(
    agent: &AgentContext,
    process: uuid::Uuid,
) -> Result<String, serde_json::Error> {
    encode_follow_on(agent, Some(process))
}

/// Whether this row belongs to a call started by `process`. Rows from older
/// binaries have no process marker and remain recoverable.
pub(crate) fn call_started_in(armed_by: &str, process: uuid::Uuid) -> bool {
    serde_json::from_str::<ArmedBy>(armed_by)
        .ok()
        .is_some_and(|armed| armed.call_process_id == Some(process))
}

fn encode_follow_on(
    agent: &AgentContext,
    process: Option<uuid::Uuid>,
) -> Result<String, serde_json::Error> {
    let mut armed = armed_by(agent);
    armed.call_process_id = process;
    armed.scheduled_follow_on = Some(ScheduledFollowOn {
        callback_hops: agent.callback_hops,
        idempotency_key: agent
            .idempotency_key
            .as_ref()
            .map(|key| key.as_str().to_string()),
    });
    serde_json::to_string(&armed)
}

fn armed_by(agent: &AgentContext) -> ArmedBy {
    ArmedBy {
        security_ctx: agent.security_ctx.clone(),
        agent_id: agent.agent_id.clone(),
        session_id: agent.session_id.clone(),
        agent_type: agent.agent_type.clone(),
        intent: agent.intent.clone(),
        trace_id: agent.trace_id.clone(),
        parent_span_id: agent.parent_span_id.clone(),
        workflow_root_entity_type: agent.workflow_root_entity_type.clone(),
        workflow_root_entity_id: agent.workflow_root_entity_id.clone(),
        workflow_run_id: agent.workflow_run_id.clone(),
        observation_metadata: agent.observation_metadata.clone(),
        read_when_set: agent.read_when_set,
        scheduled_follow_on: None,
        call_process_id: None,
    }
}

/// The identity a row keeps, as a context to fire with.
pub(crate) fn decode_armed_by(armed_by: &str) -> Result<AgentContext, serde_json::Error> {
    let armed_by: ArmedBy = serde_json::from_str(armed_by)?;
    let follow_on = armed_by.scheduled_follow_on;
    Ok(AgentContext {
        callback_hops: follow_on.as_ref().map_or(0, |follow| follow.callback_hops),
        idempotency_key: follow_on
            .and_then(|follow| follow.idempotency_key)
            .map(IdempotencyKey::FollowUp),
        security_ctx: armed_by.security_ctx,
        agent_id: armed_by.agent_id,
        session_id: armed_by.session_id,
        agent_type: armed_by.agent_type,
        intent: armed_by.intent,
        trace_id: armed_by.trace_id,
        parent_span_id: armed_by.parent_span_id,
        workflow_root_entity_type: armed_by.workflow_root_entity_type,
        workflow_root_entity_id: armed_by.workflow_root_entity_id,
        workflow_run_id: armed_by.workflow_run_id,
        observation_metadata: armed_by.observation_metadata,
        read_when_set: armed_by.read_when_set,
        ..AgentContext::default()
    })
}

#[cfg(test)]
mod tests {
    use temper_authz::{IssuedCredential, PrincipalKind, SecurityContext};

    use super::*;

    fn agent() -> SecurityContext {
        SecurityContext::from_verified_jwt(
            "agent-1",
            PrincipalKind::Agent,
            Some("contributor"),
            Some("user-1"),
            None,
            None,
        )
    }

    #[test]
    fn a_row_keeps_what_the_setters_token_was_issued_under() {
        let credential = IssuedCredential {
            subject: Some("user-1".to_string()),
            auth_generation: 2,
            grant_id: Some("grant-1".to_string()),
        };
        let setter = AgentContext {
            security_ctx: Some(agent().with_credential(Some(credential.clone()))),
            ..AgentContext::default()
        };
        for row in [
            encode_armed_by(&setter).unwrap(),
            encode_scheduled_armed_by(&setter).unwrap(),
        ] {
            let fired_as = decode_armed_by(&row).unwrap();
            assert_eq!(
                fired_as.security_ctx.and_then(|ctx| ctx.credential),
                Some(credential.clone())
            );
        }
    }

    #[test]
    fn a_row_written_before_rows_kept_a_credential_fires_without_one() {
        let setter = AgentContext {
            security_ctx: Some(agent()),
            ..AgentContext::default()
        };
        let row = encode_armed_by(&setter).unwrap();
        assert!(
            !row.contains("credential"),
            "an identity without a credential writes the row it always did: {row}"
        );
        let fired_as = decode_armed_by(&row).unwrap().security_ctx.unwrap();
        assert_eq!(fired_as.principal.id, "agent-1");
        assert!(fired_as.credential.is_none());
    }
}
