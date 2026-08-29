//! Authorization-denial persistence and trajectory emission.

use super::super::WasmEntityRef;
use crate::request_context::AgentContext;
use crate::state::pending_decisions::PendingDecision;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

impl crate::state::ServerState {
    /// Record a WASM authorization denial and emit its governed trajectory.
    pub(super) fn record_wasm_authz_denial(
        &self,
        entity_ref: WasmEntityRef<'_>,
        trigger_action: &str,
        integration_name: &str,
        module_name: &str,
        error_str: &str,
        agent_ctx: &AgentContext,
    ) -> Option<String> {
        let pd = PendingDecision::from_denial(
            entity_ref.tenant.as_str(),
            "wasm-module",
            "http_call",
            "HttpEndpoint",
            integration_name,
            serde_json::json!({
                "entity_type": entity_ref.entity_type,
                "entity_id": entity_ref.entity_id,
                "module": module_name,
                "trigger_action": trigger_action,
            }),
            error_str,
            Some(module_name.to_string()),
        );
        let decision_id = pd.id.clone();
        let _ = self.pending_decision_tx.send(pd.clone());
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::Decisions);
        let state_c = self.clone();
        tokio::spawn(async move {
            // determinism-ok: background persist of pending decision
            if let Err(e) = state_c.persist_pending_decision(&pd).await {
                tracing::error!(error = %e, "failed to persist WASM authz decision");
            }
        });

        let state_c = self.clone();
        let gd_id = format!("GD-{}", sim_uuid());
        let dispatch_ctx = AgentContext::for_service_inheriting("wasm-runtime", agent_ctx);
        let gd_params = serde_json::json!({
            "tenant": entity_ref.tenant.as_str(), "agent_id": "wasm-module",
            "action_name": "http_call", "resource_type": "HttpEndpoint",
            "resource_id": integration_name, "denial_reason": error_str,
            "scope": "narrow", "pending_decision_id": decision_id,
        });
        #[rustfmt::skip]
        tokio::spawn(async move { // determinism-ok: background entity creation
            let tenant = TenantId::new("temper-system");
            if let Err(e) = state_c.dispatch_tenant_action(
                &tenant, "GovernanceDecision", &gd_id,
                "CreateGovernanceDecision", gd_params, &dispatch_ctx,
            ).await {
                tracing::warn!(error = %e, "failed to create GovernanceDecision for WASM denial");
            }
        });

        let traj = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: entity_ref.tenant.to_string(),
            entity_type: entity_ref.entity_type.to_string(),
            entity_id: entity_ref.entity_id.to_string(),
            action: trigger_action.to_string(),
            success: false,
            from_status: None,
            to_status: None,
            error: Some(error_str.to_string()),
            agent_id: agent_ctx.agent_id.clone(),
            session_id: agent_ctx.session_id.clone(),
            authz_denied: Some(true),
            denied_resource: Some(integration_name.to_string()),
            denied_module: Some(module_name.to_string()),
            source: Some(TrajectorySource::Authz),
            spec_governed: None,
            agent_type: agent_ctx.agent_type.clone(),
            request_body: Some(serde_json::json!({
                "integration": integration_name,
                "module": module_name,
                "trigger_action": trigger_action,
            })),
            intent: agent_ctx.intent.clone(),
            matched_policy_ids: None,
            capture_seq: None,
        };
        tracing::info!(
            tenant = %traj.tenant,
            entity_type = %traj.entity_type,
            entity_id = %traj.entity_id,
            action = %traj.action,
            success = traj.success,
            from_status = ?traj.from_status,
            to_status = ?traj.to_status,
            error = ?traj.error,
            source = ?traj.source,
            authz_denied = ?traj.authz_denied,
            agent_id = traj.agent_id.as_deref().unwrap_or(""),
            session_id = traj.session_id.as_deref().unwrap_or(""),
            agent_type = traj.agent_type.as_deref().unwrap_or(""),
            intent = traj.intent.as_deref().unwrap_or(""),
            "trajectory.entry"
        );
        if !traj.success {
            tracing::warn!(
                tenant = %traj.tenant,
                entity_type = %traj.entity_type,
                entity_id = %traj.entity_id,
                action = %traj.action,
                error = ?traj.error,
                authz_denied = ?traj.authz_denied,
                source = ?traj.source,
                "unmet_intent"
            );
        }
        self.enqueue_trajectory_entry(traj);
        Some(decision_id)
    }
}
