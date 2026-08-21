use super::{
    WasmDispatchCtx, WasmDispatchMode, WasmEntityRef, is_http_call_authz_denial,
    record_wasm_error_on_current_span,
};
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::pending_decisions::PendingDecision;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use crate::state::wasm_invocation_log::WasmInvocationEntry;
use temper_observe::wide_event;
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use tracing::{Instrument, instrument};

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
        let state = self.clone();
        let persist_entry = log_entry.clone();
        let span = tracing::info_span!(
            "dispatch.phase.persist_wasm_invocation",
            otel.name = "dispatch.phase.persist_wasm_invocation",
            tenant = %persist_entry.tenant,
            entity_type = %persist_entry.entity_type,
            entity_id = %persist_entry.entity_id,
            module_name = %persist_entry.module_name,
            trigger_action = %persist_entry.trigger_action,
            success = persist_entry.success,
        );
        tokio::spawn(
            // determinism-ok: background persist of WASM invocation
            async move {
                if let Err(e) = state.persist_wasm_invocation(&persist_entry).await {
                    tracing::error!(error = %e, "failed to persist WASM invocation");
                }
            }
            .instrument(span),
        );

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

    #[instrument(skip_all, fields(
        otel.name = "dispatch.handle_wasm_failure",
        trigger_action,
        integration_name,
        module_name,
        error.type = tracing::field::Empty,
        error.message = tracing::field::Empty,
        exception.message = tracing::field::Empty,
    ))]
    pub(super) async fn handle_wasm_failure(
        &self,
        ctx: &WasmDispatchCtx<'_>,
        integration_name: &str,
        module_name: &str,
        on_failure: &Option<String>,
        error_str: String,
        duration_ms: u64,
    ) -> Result<Option<EntityResponse>, String> {
        record_wasm_error_on_current_span(&error_str);
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
                ctx.agent_ctx,
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
            return super::dispatch_wasm_callback_boxed(
                self,
                ctx.entity_ref,
                cb,
                params,
                ctx.agent_ctx,
                ctx.mode,
            )
            .await;
        }

        // No declared recovery: propagate the failure instead of swallowing it
        // (ADR-0152). The invocation was already recorded above, so telemetry
        // is preserved. Inline this surfaces as `success: false`; background
        // the dispatcher drives a compensating transition.
        Err(error_str)
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
                // Preserve inline semantics through nested WASM callbacks.
                // A public action may dispatch a validation callback that has
                // its own WASM trigger; returning before that nested trigger
                // commits lets concurrent requests observe stale detailed
                // fields while counters advance.
                let resp = super::dispatch_tenant_action_core_boxed(
                    self,
                    entity_ref.tenant,
                    entity_ref.entity_type,
                    entity_ref.entity_id,
                    callback_action,
                    callback_params,
                    agent_ctx,
                    true,
                    None,
                    None,
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok(Some(resp))
            }
            WasmDispatchMode::Background => {
                let callback_ctx = AgentContext::for_service_inheriting("wasm-runtime", agent_ctx);
                self.dispatch_tenant_action(
                    entity_ref.tenant,
                    entity_ref.entity_type,
                    entity_ref.entity_id,
                    callback_action,
                    callback_params,
                    &callback_ctx,
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
        // Tell the Observe UI a new decision exists so the Decisions tab refreshes live.
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
            // The denial belongs to the agent whose dispatch triggered the
            // WASM call. Dropping that identity left every WASM denial
            // unattributable in the trajectory stream.
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
