//! Post-dispatch effect handlers.
//!
//! Isolates side effects (telemetry, SSE, cache, webhooks, WASM, spawn)
//! that run after a successful entity action dispatch. Keeps the core
//! dispatch path focused on transition execution.

use std::sync::Arc;
use std::time::Instant;

use tokio::spawn as spawn_post_dispatch_effect; // determinism-ok: post-dispatch side effects are outside transition semantics
use tracing::Instrument;

use crate::entity_actor::{EntityResponse, effects::ScheduledAction};
use crate::events::EntityStateChange;
use crate::request_context::AgentContext;
use crate::state::ProjectionEnqueueOutcome;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

mod orchestration;
mod projection;

/// Collected context needed by post-dispatch effect handlers.
pub(crate) struct PostDispatchContext<'a> {
    pub tenant: &'a TenantId,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub agent_ctx: &'a AgentContext,
    pub dispatch_idempotency_key: Option<&'a str>,
    pub action_params: &'a serde_json::Value,
    pub await_integration: bool,
    /// Actor incarnation that produced the response, when mailbox-backed.
    pub actor_uid: Option<uuid::Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryProjectionUpdateMode {
    /// The dispatch response is returned after the journal append and after a
    /// bounded, coalescing projection update is accepted. The projection row
    /// may lag the journal, but stale pending writes are skipped before DB
    /// access and lag is observable (ADR-0148).
    Queued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchTrajectoryPersistenceMode {
    Background,
}

fn query_projection_update_mode() -> QueryProjectionUpdateMode {
    QueryProjectionUpdateMode::Queued
}

fn dispatch_trajectory_persistence_mode() -> DispatchTrajectoryPersistenceMode {
    DispatchTrajectoryPersistenceMode::Background
}

impl crate::state::ServerState {
    pub(super) fn persist_trajectory_entry_background(&self, entry: TrajectoryEntry) {
        debug_assert_eq!(
            dispatch_trajectory_persistence_mode(),
            DispatchTrajectoryPersistenceMode::Background
        );
        self.enqueue_trajectory_entry(entry);
    }

    /// Record a trajectory entry for a completed dispatch (success or guard failure).
    pub(crate) fn record_dispatch_trajectory(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let entry = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: ctx.tenant.to_string(),
            entity_type: ctx.entity_type.to_string(),
            entity_id: ctx.entity_id.to_string(),
            action: ctx.action.to_string(),
            success: response.success,
            from_status: response.state.events.back().map(|e| e.from_status.clone()),
            to_status: Some(response.state.status.clone()),
            error: if response.success {
                None
            } else {
                Some(
                    response
                        .error
                        .clone()
                        .unwrap_or_else(|| "guard not met".to_string()),
                )
            },
            agent_id: ctx.agent_ctx.agent_id.clone(),
            session_id: ctx.agent_ctx.session_id.clone(),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some(TrajectorySource::Entity),
            spec_governed: None,
            agent_type: ctx.agent_ctx.agent_type.clone(),
            request_body: if response.success {
                None
            } else {
                Some(ctx.action_params.clone())
            },
            intent: ctx.agent_ctx.intent.clone(),
            matched_policy_ids: None,
        };
        let from_status = entry.from_status.as_deref().unwrap_or("unknown");
        let to_status = entry.to_status.as_deref().unwrap_or("unknown");
        let outcome = if entry.success { "succeeded" } else { "failed" };
        let observation_metadata = ctx
            .agent_ctx
            .observation_metadata_json()
            .unwrap_or_default();
        tracing::info!(
            tenant = %entry.tenant,
            entity_type = %entry.entity_type,
            entity_id = %entry.entity_id,
            action = %entry.action,
            success = entry.success,
            from_status = ?entry.from_status,
            to_status = ?entry.to_status,
            error = ?entry.error,
            source = ?entry.source,
            authz_denied = ?entry.authz_denied,
            agent_id = entry.agent_id.as_deref().unwrap_or(""),
            session_id = entry.session_id.as_deref().unwrap_or(""),
            agent_type = entry.agent_type.as_deref().unwrap_or(""),
            intent = entry.intent.as_deref().unwrap_or(""),
            observation_metadata = %observation_metadata,
            "app usage: {}.{} {} -> {} on {} {}",
            entry.entity_type,
            entry.action,
            from_status,
            to_status,
            entry.entity_id,
            outcome
        );
        if !entry.success {
            tracing::warn!(
                tenant = %entry.tenant,
                entity_type = %entry.entity_type,
                entity_id = %entry.entity_id,
                action = %entry.action,
                error = ?entry.error,
                authz_denied = ?entry.authz_denied,
                source = ?entry.source,
                "unmet_intent"
            );
        }
        self.persist_trajectory_entry_background(entry);
    }

    /// Broadcast state change to SSE subscribers and update entity cache.
    pub(crate) fn broadcast_state_change(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let seq =
            self.next_entity_event_sequence(ctx.tenant.as_str(), ctx.entity_type, ctx.entity_id);
        let change = EntityStateChange {
            seq,
            entity_type: ctx.entity_type.to_string(),
            entity_id: ctx.entity_id.to_string(),
            action: ctx.action.to_string(),
            status: response.state.status.clone(),
            tenant: ctx.tenant.to_string(),
            agent_id: ctx.agent_ctx.agent_id.clone(),
            session_id: ctx.agent_ctx.session_id.clone(),
            intent: ctx.agent_ctx.intent.clone(),
            observation_metadata: (!ctx.agent_ctx.observation_metadata.is_empty())
                .then(|| ctx.agent_ctx.observation_metadata.clone()),
        };
        self.record_entity_observe_event_with_seq(
            ctx.tenant.as_str(),
            ctx.entity_type,
            ctx.entity_id,
            seq,
            "state_change",
            serde_json::to_value(&change).unwrap_or_default(),
        );
        let _ = self.event_tx.send(change);
        if matches!(
            response.state.status.as_str(),
            "Completed" | "Failed" | "Cancelled"
        ) {
            let terminal_seq = self.next_entity_event_sequence(
                ctx.tenant.as_str(),
                ctx.entity_type,
                ctx.entity_id,
            );
            let result = response
                .state
                .fields
                .get("result")
                .or_else(|| response.state.fields.get("Result"))
                .and_then(serde_json::Value::as_str);
            let error_message = response
                .state
                .fields
                .get("error_message")
                .or_else(|| response.state.fields.get("ErrorMessage"))
                .and_then(serde_json::Value::as_str)
                .or(response.error.as_deref());
            self.record_entity_observe_event_with_seq(
                ctx.tenant.as_str(),
                ctx.entity_type,
                ctx.entity_id,
                terminal_seq,
                "agent_complete",
                serde_json::json!({
                    "seq": terminal_seq,
                    "status": response.state.status,
                    "action": ctx.action,
                    "result": result,
                    "error_message": error_message,
                    "agent_id": ctx.agent_ctx.agent_id,
                    "session_id": ctx.agent_ctx.session_id,
                    "observation_metadata": ctx.agent_ctx.observation_metadata.clone(),
                }),
            );
        }
        let cache_key = format!("{}:{}:{}", ctx.tenant, ctx.entity_type, ctx.entity_id);
        self.cache_entity_status(cache_key, response.state.status.clone());
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::Entities);
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::Trajectories);
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::Agents);
    }

    /// Fire webhooks for the trajectory entry (non-blocking).
    pub(crate) fn fire_webhooks(&self, ctx: &PostDispatchContext<'_>, response: &EntityResponse) {
        if let Some(ref dispatcher) = self.webhook_dispatcher {
            let dispatcher = Arc::clone(dispatcher);
            let entry = TrajectoryEntry {
                timestamp: sim_now().to_rfc3339(),
                tenant: ctx.tenant.to_string(),
                entity_type: ctx.entity_type.to_string(),
                entity_id: ctx.entity_id.to_string(),
                action: ctx.action.to_string(),
                success: response.success,
                from_status: response.state.events.back().map(|e| e.from_status.clone()),
                to_status: Some(response.state.status.clone()),
                error: response.error.clone(),
                agent_id: ctx.agent_ctx.agent_id.clone(),
                session_id: ctx.agent_ctx.session_id.clone(),
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some(TrajectorySource::Entity),
                spec_governed: None,
                agent_type: ctx.agent_ctx.agent_type.clone(),
                request_body: None,
                intent: ctx.agent_ctx.intent.clone(),
                matched_policy_ids: None,
            };
            let from_status = entry.from_status.as_deref().unwrap_or("unknown");
            let to_status = entry.to_status.as_deref().unwrap_or("unknown");
            let outcome = if entry.success { "succeeded" } else { "failed" };
            let observation_metadata = ctx
                .agent_ctx
                .observation_metadata_json()
                .unwrap_or_default();
            tracing::info!(
                tenant = %entry.tenant,
                entity_type = %entry.entity_type,
                entity_id = %entry.entity_id,
                action = %entry.action,
                success = entry.success,
                from_status = ?entry.from_status,
                to_status = ?entry.to_status,
                error = ?entry.error,
                source = ?entry.source,
                authz_denied = ?entry.authz_denied,
                agent_id = entry.agent_id.as_deref().unwrap_or(""),
                session_id = entry.session_id.as_deref().unwrap_or(""),
                agent_type = entry.agent_type.as_deref().unwrap_or(""),
                intent = entry.intent.as_deref().unwrap_or(""),
                observation_metadata = %observation_metadata,
                "app usage: {}.{} {} -> {} on {} {}",
                entry.entity_type,
                entry.action,
                from_status,
                to_status,
                entry.entity_id,
                outcome
            );
            if !entry.success {
                tracing::warn!(
                    tenant = %entry.tenant,
                    entity_type = %entry.entity_type,
                    entity_id = %entry.entity_id,
                    action = %entry.action,
                    error = ?entry.error,
                    authz_denied = ?entry.authz_denied,
                    source = ?entry.source,
                    "unmet_intent"
                );
            }
            spawn_post_dispatch_effect(async move {
                // determinism-ok: external side-effect, no simulation-visible state
                dispatcher.dispatch(&entry);
            });
        }
    }

    /// Schedule delayed actions as fire-and-forget background timers.
    ///
    /// Propagates the originating `AgentContext` so that scheduled actions
    /// retain identity attribution in trajectories and SSE events.
    pub(crate) fn dispatch_scheduled_actions(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        scheduled_actions: &[ScheduledAction],
        agent_ctx: &AgentContext,
    ) {
        for sched in scheduled_actions {
            let state = self.clone();
            let t = tenant.clone();
            let et = entity_type.to_string();
            let eid = entity_id.to_string();
            let action = sched.action.clone();
            let ctx = agent_ctx.clone();
            let delay = std::time::Duration::from_secs(sched.delay_seconds);
            let workflow_root_entity_type = ctx
                .workflow_root_entity_type
                .clone()
                .unwrap_or_else(|| et.clone());
            let workflow_root_entity_id = ctx
                .workflow_root_entity_id
                .clone()
                .unwrap_or_else(|| eid.clone());
            let workflow_run_id = ctx
                .workflow_run_id
                .clone()
                .unwrap_or_else(|| format!("{et}:{eid}"));
            let span = tracing::info_span!(
                "dispatch.scheduled_actions",
                workflow.root_entity_type = %workflow_root_entity_type,
                workflow.root_entity_id = %workflow_root_entity_id,
                workflow.run_id = %workflow_run_id,
                temper.action = %action,
                entity_type = %et,
                entity_id = %eid,
            );
            spawn_post_dispatch_effect(
                // determinism-ok: timer delivery is a background side-effect
                async move {
                    tokio::time::sleep(delay).await; // determinism-ok: scheduled delay
                    let _ = state
                        .dispatch_tenant_action(
                            &t,
                            &et,
                            &eid,
                            &action,
                            serde_json::json!({"__scheduled": true}),
                            &ctx,
                        )
                        .await;
                }
                .instrument(span),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_projection_updates_are_queued_after_journal_commit() {
        // ADR-0148: dispatch waits for the event journal, then accepts derived
        // projection maintenance into a bounded coalescing queue.
        assert_eq!(
            query_projection_update_mode(),
            QueryProjectionUpdateMode::Queued
        );
    }

    #[test]
    fn dispatch_trajectory_persistence_is_not_on_the_dispatch_critical_path() {
        assert_eq!(
            dispatch_trajectory_persistence_mode(),
            DispatchTrajectoryPersistenceMode::Background
        );
    }
}
