//! Reporting a reaction the dispatcher stops because it would repeat a step
//! of its chain.

use temper_runtime::tenant::TenantId;

use crate::request_context::RepeatedStep;

/// A reaction that was not run because it would repeat a step of its chain,
/// on the entity whose action would have fired it.
pub(super) struct RepeatStop<'a> {
    pub(super) tenant: &'a TenantId,
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) action: &'a str,
    pub(super) rule: &'a super::types::ReactionRule,
    pub(super) target_entity_id: &'a str,
    pub(super) repeat: &'a RepeatedStep,
    pub(super) depth: u32,
}

impl RepeatStop<'_> {
    /// Say so on the source entity's observe stream, where the app and its
    /// agents watch it, as well as in the log.
    pub(super) fn record(self, state: &crate::ServerState) {
        let seq = state.next_entity_event_sequence(
            self.tenant.as_str(),
            self.entity_type,
            self.entity_id,
        );
        state.record_entity_observe_event_with_seq(
            self.tenant.as_str(),
            self.entity_type,
            self.entity_id,
            seq,
            "reaction_cycle_stopped",
            serde_json::json!({
                "action": self.action,
                "rule": self.rule.name,
                "target_entity_type": self.rule.then.entity_type,
                "target_entity_id": self.target_entity_id,
                "target_action": self.rule.then.action,
                "cycle": self.repeat.path(),
                "depth": self.depth,
            }),
        );
        tracing::warn!(
            tenant = %self.tenant,
            entity_type = self.entity_type,
            entity_id = self.entity_id,
            action = self.action,
            rule = self.rule.name,
            target_entity = %self.rule.then.entity_type,
            target_id = self.target_entity_id,
            depth = self.depth,
            cycle = %self.repeat,
            "reaction not run: it would repeat a step its chain is already in"
        );
    }
}
