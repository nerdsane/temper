//! Timeout-declaration hot-swap reconciliation.

use super::*;

impl crate::state::ServerState {
    pub(super) async fn reconcile_state_timeout_after_table_change(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        agent_ctx: &crate::request_context::AgentContext,
    ) -> Result<(), String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor_when_ready(tenant, entity_type, entity_id)
            .await
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;
        let policy = self.dispatch_retry_policy();
        let (actor_ref, outcome) = self
            .ask_actor_with_drain_retry::<EntityResponse, _>(
                tenant,
                entity_type,
                entity_id,
                actor_ref,
                || EntityMsg::GetState,
                &policy,
            )
            .await;
        let actor_uid = actor_ref.id().uid;
        let response = outcome
            .result
            .map_err(|error| format!("Actor query failed: {error}"))?;
        let action_params = serde_json::json!({});
        let ctx = PostDispatchContext {
            tenant,
            entity_type,
            entity_id,
            action: "__table_changed",
            agent_ctx,
            dispatch_idempotency_key: None,
            action_params: &action_params,
            await_integration: false,
            actor_uid: Some(actor_uid),
        };
        self.arm_state_timeouts_if_needed(&ctx, &response);
        Ok(())
    }

    pub(super) async fn reconcile_state_timeout_declaration_until_current(
        &self,
        table_versions: &mut tokio::sync::watch::Receiver<u64>,
        sender_closed: &mut bool,
        cancellation: &mut tokio::sync::watch::Receiver<bool>,
        watch: &StateTimeoutWatch<'_>,
    ) -> bool {
        let mut failure_count = 0_u32;
        loop {
            if *cancellation.borrow() {
                return false;
            }
            if self.state_timeout_tracker.current_generation(watch.key) != watch.armed_generation {
                return false;
            }

            match self.current_state_timeout_declaration(
                watch.tenant,
                watch.entity_type,
                watch.target_state,
            ) {
                Some(current) if current == *watch.expected_timeout => return true,
                None => {
                    let _ = self
                        .state_timeout_tracker
                        .invalidate_generation_if_current(watch.key, watch.armed_generation);
                    return false;
                }
                Some(_) => {}
            }

            let reconcile_error = self
                .reconcile_state_timeout_after_table_change(
                    watch.tenant,
                    watch.entity_type,
                    watch.entity_id,
                    watch.agent_ctx,
                )
                .await
                .err();
            #[cfg(test)]
            if reconcile_error.is_some() {
                self.state_timeout_tracker.record_reconciliation_failure();
            }
            if self.state_timeout_tracker.current_generation(watch.key) != watch.armed_generation {
                return false;
            }

            match self.current_state_timeout_declaration(
                watch.tenant,
                watch.entity_type,
                watch.target_state,
            ) {
                Some(current) if current == *watch.expected_timeout => return true,
                None => {
                    let _ = self
                        .state_timeout_tracker
                        .invalidate_generation_if_current(watch.key, watch.armed_generation);
                    return false;
                }
                Some(_) => {}
            }

            failure_count = failure_count.saturating_add(1);
            let retry_delay = state_timeout_retry_delay(failure_count, None);
            tracing::warn!(
                tenant = %watch.tenant,
                entity_type = watch.entity_type,
                entity_id = watch.entity_id,
                failure_count,
                error = reconcile_error.as_deref().unwrap_or("ownership did not advance"),
                retry_delay_ms = retry_delay.as_millis() as u64,
                "state timeout table reconciliation will retry"
            );
            let retry_deadline = timeout_deadline(retry_delay);
            if *sender_closed {
                if !wait_until_or_cancelled(cancellation, retry_deadline).await {
                    return false;
                }
                continue;
            }

            tokio::select! { // determinism-ok: ordered reconciliation retry and version signal
                biased;
                _ = cancellation.changed() => return false,
                changed = table_versions.changed() => {
                    *sender_closed = changed.is_err();
                }
                _ = tokio::time::sleep_until(retry_deadline) => {} // determinism-ok: bounded reconciliation retry
            }
        }
    }

    pub(super) async fn wait_for_state_timeout_deadline(
        &self,
        table_versions: Option<&mut tokio::sync::watch::Receiver<u64>>,
        cancellation: &mut tokio::sync::watch::Receiver<bool>,
        deadline: tokio::time::Instant,
        watch: &StateTimeoutWatch<'_>,
    ) -> bool {
        let Some(table_versions) = table_versions else {
            return wait_until_or_cancelled(cancellation, deadline).await;
        };

        loop {
            tokio::select! { // determinism-ok: ordered timer and table-version signal
                biased;
                _ = cancellation.changed() => return false,
                changed = table_versions.changed() => {
                    let mut sender_closed = changed.is_err();
                    if self.state_timeout_tracker.current_generation(watch.key)
                        != watch.armed_generation
                    {
                        return false;
                    }
                    let current_declaration = self.current_state_timeout_declaration(
                        watch.tenant,
                        watch.entity_type,
                        watch.target_state,
                    );
                    if current_declaration.as_ref() == Some(watch.expected_timeout) {
                        if sender_closed {
                            return wait_until_or_cancelled(cancellation, deadline).await;
                        }
                        continue;
                    }
                    if !self
                        .reconcile_state_timeout_declaration_until_current(
                            table_versions,
                            &mut sender_closed,
                            cancellation,
                            watch,
                        )
                        .await
                    {
                        return false;
                    }
                    if sender_closed {
                        return wait_until_or_cancelled(cancellation, deadline).await;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => { // determinism-ok: scheduled deadline
                    return !*cancellation.borrow();
                }
            }
        }
    }
}
