use super::{WasmDispatchCtx, WasmDispatchMode, WasmEntityRef, is_http_call_authz_denial};
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::pending_decisions::PendingDecision;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use crate::state::wasm_invocation_log::WasmInvocationEntry;
use temper_observe::wide_event;
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use tracing::instrument;

impl crate::state::ServerState {
    /// Record a WASM invocation (persist log entry + emit observability events).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn record_invocation(
        &self,
        entity_ref: WasmEntityRef<'_>,
        module_name: &str,
        trigger_action: &str,
        callback_action: Option<String>,
        success: bool,
        error: Option<String>,
        duration_ms: u64,
        authz_denied: Option<bool>,
    ) {
        let log_entry = WasmInvocationEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: entity_ref.tenant.to_string(),
            entity_type: entity_ref.entity_type.to_string(),
            entity_id: entity_ref.entity_id.to_string(),
            module_name: module_name.to_string(),
            trigger_action: trigger_action.to_string(),
            callback_action,
            success,
            error: error.clone(),
            duration_ms,
            authz_denied,
        };
        let _ = self.persist_wasm_invocation(&log_entry).await;

        let wide = wide_event::from_wasm_invocation(wide_event::WasmInvocationInput {
            module_name,
            trigger_action,
            entity_type: entity_ref.entity_type,
            entity_id: entity_ref.entity_id,
            tenant: &entity_ref.tenant.to_string(),
            success,
            duration_ns: duration_ms * 1_000_000,
            error: error.as_deref(),
        });
        wide_event::emit_span(&wide);
        wide_event::emit_metrics(&wide);
    }

    #[instrument(skip_all, fields(otel.name = "dispatch.handle_wasm_failure", trigger_action, integration_name, module_name))]
    pub(super) async fn handle_wasm_failure(
        &self,
        ctx: &WasmDispatchCtx<'_>,
        integration_name: &str,
        module_name: &str,
        on_failure: &Option<String>,
        error_str: String,
        duration_ms: u64,
    ) -> Result<Option<EntityResponse>, String> {
        let is_authz_denied = is_http_call_authz_denial(&error_str);
        self.record_invocation(
            ctx.entity_ref,
            module_name,
            ctx.action,
            on_failure.clone(),
            false,
            Some(error_str.clone()),
            duration_ms,
            if is_authz_denied { Some(true) } else { None },
        )
        .await;

        let decision_id = if is_authz_denied {
            self.record_wasm_authz_denial(
                ctx.entity_ref,
                ctx.action,
                integration_name,
                module_name,
                &error_str,
            )
        } else {
            None
        };

        if let Some(cb) = on_failure {
            let mut params = serde_json::json!({
                "error": error_str.clone(),
                "error_message": error_str,
                "integration": integration_name,
            });
            if let Some(ref did) = decision_id {
                params["decision_id"] = serde_json::json!(did);
                params["authz_denied"] = serde_json::json!(true);
            }
            return self
                .dispatch_wasm_callback(ctx.entity_ref, cb, params, ctx.agent_ctx, ctx.mode)
                .await;
        }

        Ok(None)
    }

    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_wasm_callback", callback_action))]
    pub(super) async fn dispatch_wasm_callback(
        &self,
        entity_ref: WasmEntityRef<'_>,
        callback_action: &str,
        callback_params: serde_json::Value,
        agent_ctx: &AgentContext,
        mode: WasmDispatchMode,
    ) -> Result<Option<EntityResponse>, String> {
        match mode {
            WasmDispatchMode::Inline => {
                let resp = self
                    .dispatch_tenant_action_core(
                        entity_ref.tenant,
                        entity_ref.entity_type,
                        entity_ref.entity_id,
                        callback_action,
                        callback_params,
                        agent_ctx,
                        false,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Some(resp))
            }
            WasmDispatchMode::Background => {
                self.dispatch_tenant_action(
                    entity_ref.tenant,
                    entity_ref.entity_type,
                    entity_ref.entity_id,
                    callback_action,
                    callback_params,
                    &AgentContext::system(),
                )
                .await
                .map_err(|e| {
                    let msg = format!("failed to dispatch WASM callback '{callback_action}': {e}");
                    tracing::error!(callback = %callback_action, error = %e, "{msg}");
                    msg
                })?;
                Ok(None)
            }
        }
    }

    /// Record a WASM authorization denial: persist decision, create governance
    /// entity, and emit trajectory entry.
    pub(super) fn record_wasm_authz_denial(
        &self,
        entity_ref: WasmEntityRef<'_>,
        trigger_action: &str,
        integration_name: &str,
        module_name: &str,
        error_str: &str,
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
        let state_c = self.clone();
        #[rustfmt::skip]
        tokio::spawn(async move { // determinism-ok: background persist
            if let Err(e) = state_c.persist_pending_decision(&pd).await {
                tracing::error!(error = %e, "failed to persist WASM authz decision");
            }
        });

        let state_c = self.clone();
        let gd_id = format!("GD-{}", sim_uuid());
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
                "CreateGovernanceDecision", gd_params, &AgentContext::system(),
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
            agent_id: None,
            session_id: None,
            authz_denied: Some(true),
            denied_resource: Some(integration_name.to_string()),
            denied_module: Some(module_name.to_string()),
            source: Some(TrajectorySource::Authz),
            spec_governed: None,
            agent_type: None,
            request_body: None,
            intent: None,
            matched_policy_ids: None,
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
        let state_c = self.clone();
        #[rustfmt::skip]
        tokio::spawn(async move { // determinism-ok: background persist
            if let Err(e) = state_c.persist_trajectory_entry(&traj).await {
                tracing::error!(error = %e, "failed to persist WASM authz trajectory");
            }
        });
        Some(decision_id)
    }
}
