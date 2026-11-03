//! The signal left by a reaction that did not move its target, on its first
//! try or a retry: Cedar refused it, or the target's spec did. The source move
//! stands either way, so without this signal the only trace is a record that
//! did not move.

use std::collections::BTreeMap;

use temper_runtime::scheduler::sim_now;

use super::owed_reactions::Reaction;
use crate::ServerState;
use crate::entity_actor::EntityResponse;
use crate::state::{TrajectoryEntry, TrajectorySource};

impl Reaction {
    pub(super) fn record_withheld(
        &self,
        state: &ServerState,
        status: &str,
        resource_attrs: &BTreeMap<String, serde_json::Value>,
        reason: &str,
    ) {
        let sent = &self.sent;
        let principal = &self.authorized_as.principal;
        let cause = self
            .authorized_as
            .cause
            .as_ref()
            .expect("a reaction carries its cause");
        let hop = cause
            .reaction
            .as_ref()
            .expect("a reaction carries its source move");
        tracing::warn!(
            tenant = %sent.tenant,
            entity_type = sent.entity_type,
            entity_id = sent.entity_id,
            action = sent.action,
            status,
            rule = hop.rule,
            source_entity_type = hop.entity_type,
            source_entity_id = hop.entity_id,
            source_action = hop.action,
            principal_id = principal.id,
            principal_kind = ?principal.kind,
            acting_for = ?principal.acting_for,
            via = cause.via(),
            depth = cause.depth,
            armed_by = ?cause.timer.as_ref().map(|arm| arm.armed_by.as_str()),
            reason,
            "reaction withheld"
        );
        crate::runtime_metrics::record_reaction_withheld(
            sent.tenant.as_str(),
            &sent.entity_type,
            &sent.action,
        );
        let entry = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: sent.tenant.to_string(),
            entity_type: sent.entity_type.clone(),
            entity_id: sent.entity_id.clone(),
            action: sent.action.clone(),
            success: false,
            from_status: Some(status.to_string()),
            to_status: None,
            error: Some(format!("reaction withheld: {reason}")),
            agent_id: Some(principal.id.clone()),
            session_id: sent.agent_ctx.session_id.clone(),
            authz_denied: Some(true),
            denied_resource: Some(format!("{}:{}", sent.entity_type, sent.entity_id)),
            denied_module: None,
            source: Some(TrajectorySource::Authz),
            spec_governed: Some(true),
            agent_type: principal.agent_type.clone(),
            request_body: Some(serde_json::to_value(resource_attrs).unwrap_or_default()),
            intent: sent.agent_ctx.intent.clone(),
            matched_policy_ids: None,
            capture_seq: None,
        };
        if !state.enqueue_trajectory_entry(entry) {
            tracing::warn!("failed to enqueue withheld reaction trajectory");
        }
    }

    /// The target's spec refused the move: its state, a guard, a constraint.
    /// Say so on the source record's observe stream, where the app and its
    /// agents watch it, as well as in the log. The refused dispatch wrote its
    /// own trajectory entry.
    pub(super) fn record_refused(&self, state: &ServerState, response: &EntityResponse) {
        let sent = &self.sent;
        let cause = self
            .authorized_as
            .cause
            .as_ref()
            .expect("a reaction carries its cause");
        let hop = cause
            .reaction
            .as_ref()
            .expect("a reaction carries its source move");
        let reason = response.error.as_deref().unwrap_or("no reason given");
        let target_status = &response.state.status;
        let tenant = sent.tenant.as_str();
        let seq = state.next_entity_event_sequence(tenant, &hop.entity_type, &hop.entity_id);
        state.record_entity_observe_event_with_seq(
            tenant,
            &hop.entity_type,
            &hop.entity_id,
            seq,
            "reaction_refused",
            serde_json::json!({
                "action": hop.action,
                "rule": hop.rule,
                "target_entity_type": sent.entity_type,
                "target_entity_id": sent.entity_id,
                "target_action": sent.action,
                "target_status": target_status,
                "refusal": response.refusal,
                "reason": reason,
                "depth": cause.depth,
            }),
        );
        tracing::warn!(
            tenant = %sent.tenant,
            entity_type = sent.entity_type,
            entity_id = sent.entity_id,
            action = sent.action,
            status = target_status.as_str(),
            rule = hop.rule,
            source_entity_type = hop.entity_type,
            source_entity_id = hop.entity_id,
            source_action = hop.action,
            refusal = ?response.refusal,
            depth = cause.depth,
            reason,
            "reaction refused"
        );
    }
}
