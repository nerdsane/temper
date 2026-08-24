//! Production dispatcher for cross-entity reactions.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use temper_authz::SecurityContext;
use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
use temper_runtime::tenant::TenantId;

use super::registry::ReactionRegistry;

mod durable;
mod fanout;

pub(crate) fn effective_trigger_security_context(
    agent_ctx: &crate::request_context::AgentContext,
) -> SecurityContext {
    if let Some(security_ctx) = &agent_ctx.security_ctx {
        return security_ctx.clone();
    }

    let mut security_ctx = SecurityContext::anonymous().with_agent_context(
        agent_ctx.agent_id.as_deref(),
        agent_ctx.session_id.as_deref(),
        agent_ctx.agent_type.as_deref(),
    );
    security_ctx.context_attrs.insert(
        "triggerInheritedContextApproximate".to_string(),
        serde_json::Value::Bool(true),
    );
    security_ctx
}

/// Snapshot cross-entity guard inputs before a source transition commits.
pub(crate) async fn resolve_rule_guard_inputs(
    state: &crate::ServerState,
    tenant: &TenantId,
    rules: &[super::types::ReactionRule],
    source_fields: &serde_json::Value,
    schema_pin: Option<&SchemaExecutionPin>,
) -> BTreeMap<String, crate::trigger::guard::CrossStatusMap> {
    let mut by_rule = BTreeMap::new();
    for rule in rules {
        let Some(guard) = rule.when.guard.as_ref() else {
            continue;
        };
        let mut queries = Vec::new();
        crate::trigger::guard::collect_cross_entity_queries(guard, source_fields, &mut queries);
        let mut resolved = crate::trigger::guard::CrossStatusMap::new();
        for query in queries {
            let status = match schema_pin {
                Some(pin) => state
                    .get_scoped_entity_state(
                        tenant,
                        &query.entity_type,
                        &query.target_entity_id,
                        pin.clone(),
                    )
                    .await
                    .ok()
                    .map(|response| response.state.status),
                None => {
                    state
                        .resolve_entity_status(tenant, &query.entity_type, &query.target_entity_id)
                        .await
                }
            };
            resolved.insert(
                query.key(),
                status.as_deref().is_some_and(|value| query.matches(value)),
            );
        }
        by_rule.insert(rule.name.clone(), resolved);
    }
    by_rule
}

#[derive(Clone)]
pub(super) struct BoundDelivery {
    delivery_id: String,
    root_delivery_id: String,
    fencing_token: u64,
    target_entity_id: Option<String>,
    expected_target_sequence: Option<u64>,
    state_timeout_state: Option<String>,
}

/// Async reaction dispatcher for production use.
///
/// Holds a shared ReactionRegistry and dispatches target actions through the
/// server state. Cascade is bounded by the reaction depth budget.
pub struct ReactionDispatcher {
    pub(super) registry: Arc<ReactionRegistry>,
    recovery_cursors: Mutex<BTreeMap<TenantId, RecoveryCursor>>,
    recovery_locks: Mutex<BTreeMap<TenantId, Arc<tokio::sync::Mutex<()>>>>,
    recovery_notify: tokio::sync::Notify,
    recovery_wake_tenants: Mutex<BTreeSet<TenantId>>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct RecoveryCursor {
    pub(super) after_journal: Option<(String, String)>,
    pub(super) current_journal: Option<(String, String)>,
    pub(super) queued_journals: VecDeque<(String, String)>,
    pub(super) event_sequence: u64,
    pub(super) intent_offset: usize,
    pub(super) next_wakeup: Option<chrono::DateTime<chrono::Utc>>,
}

impl ReactionDispatcher {
    /// Create a new dispatcher with the given registry.
    pub fn new(registry: Arc<ReactionRegistry>) -> Self {
        Self {
            registry,
            recovery_cursors: Mutex::new(BTreeMap::new()),
            recovery_locks: Mutex::new(BTreeMap::new()),
            recovery_notify: tokio::sync::Notify::new(),
            recovery_wake_tenants: Mutex::new(BTreeSet::new()),
        }
    }

    /// Snapshot every rule that may fire for a source action before it commits.
    pub(crate) fn candidate_rules(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
    ) -> Vec<super::types::ReactionRule> {
        self.registry
            .candidates(tenant, entity_type, action)
            .into_iter()
            .cloned()
            .collect()
    }

    pub(super) fn recovery_cursor(&self, tenant: &TenantId) -> RecoveryCursor {
        self.recovery_cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tenant)
            .cloned()
            .unwrap_or_default()
    }

    pub(super) fn recovery_lock(&self, tenant: &TenantId) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.recovery_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(tenant.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    pub(super) fn set_recovery_cursor(&self, tenant: &TenantId, cursor: RecoveryCursor) {
        self.recovery_cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(tenant.clone(), cursor);
    }

    pub(super) fn recovery_scan_in_progress(&self, tenant: &TenantId) -> bool {
        let cursor = self.recovery_cursor(tenant);
        cursor.after_journal.is_some()
            || cursor.current_journal.is_some()
            || !cursor.queued_journals.is_empty()
            || cursor.event_sequence != 0
            || cursor.intent_offset != 0
    }

    pub(crate) fn notify_recovery(&self, tenant: &TenantId) {
        self.recovery_wake_tenants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(tenant.clone());
        self.recovery_notify.notify_one();
    }

    pub(crate) fn take_recovery_wake_tenants(&self) -> Vec<TenantId> {
        let mut tenants = self
            .recovery_wake_tenants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *tenants).into_iter().collect()
    }

    pub(crate) async fn wait_for_recovery_signal(&self) {
        self.recovery_notify.notified().await;
    }

    pub(crate) fn recovery_supervisor_delay(&self, tenant: &TenantId) -> std::time::Duration {
        if self.recovery_scan_in_progress(tenant) {
            return std::time::Duration::from_millis(10);
        }
        self.recovery_cursor(tenant)
            .next_wakeup
            .map(|next| {
                next.signed_duration_since(temper_runtime::scheduler::sim_now())
                    .to_std()
                    .unwrap_or_default()
                    .min(std::time::Duration::from_secs(30))
            })
            .unwrap_or_else(|| std::time::Duration::from_secs(30))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_notifications_are_tenant_scoped_and_deduplicated() {
        let dispatcher = ReactionDispatcher::new(Arc::new(ReactionRegistry::new()));
        let first = TenantId::new("first");
        let second = TenantId::new("second");
        dispatcher.notify_recovery(&second);
        dispatcher.notify_recovery(&first);
        dispatcher.notify_recovery(&second);
        assert_eq!(dispatcher.take_recovery_wake_tenants(), vec![first, second]);
        assert!(dispatcher.take_recovery_wake_tenants().is_empty());
    }
}
