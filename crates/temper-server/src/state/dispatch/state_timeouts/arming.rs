//! Timeout ownership, deadline creation, and delivery retries.

use super::*;

impl crate::state::ServerState {
    pub(super) fn arm_state_timeouts(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
        cause: StateTimeoutArmCause,
    ) {
        let (registry_table, table_versions) = {
            let registry = match self.registry.read() {
                Ok(registry) => registry,
                Err(_) => return,
            };
            (
                registry.get_table(ctx.tenant, ctx.entity_type),
                registry.subscribe_table_versions(ctx.tenant, ctx.entity_type),
            )
        };
        let table = registry_table.or_else(|| self.transition_tables.get(ctx.entity_type).cloned());
        let Some(table) = table else {
            let key = EntityKey::new(ctx.tenant, ctx.entity_type, ctx.entity_id);
            let _ = self.state_timeout_tracker.invalidate_if_fresh(
                &key,
                timeout_response_order(&response.state),
                response.state.state_timeout_clock_reset_at,
                response.state.state_timeout_clock_reset_version,
            );
            return;
        };
        let state_timeouts = table.state_timeouts.clone();

        let post_state = response.state.status.clone();
        let pre_state = response
            .state
            .events
            .back()
            .map(|e| e.from_status.clone())
            .unwrap_or_default();
        let state_changed = pre_state != post_state;
        let hydrating = matches!(cause, StateTimeoutArmCause::Hydration { .. });
        let key = EntityKey::new(ctx.tenant, ctx.entity_type, ctx.entity_id);
        let post_timeout = state_timeouts.iter().find(|st| st.state == post_state);
        let post_has_timeout = post_timeout.is_some();
        let pre_had_timeout = state_timeouts.iter().any(|st| st.state == pre_state);
        let event_order = timeout_response_order(&response.state);
        // The durable event history determines the deadline below, but the
        // actor's actual clock fields are the identity validated atomically at
        // dispatch. Keeping those concerns separate lets hydration reconcile a
        // table hot-swap even when startup committed under the prior table.
        let armed_reset_at = response.state.state_timeout_clock_reset_at;
        let armed_reset_version = response.state.state_timeout_clock_reset_version;

        // A hot-swap can remove the current state's declaration without a
        // domain transition. Invalidate the captured declaration at the same
        // durable event order so an already-armed or retrying task terminates.
        if !post_has_timeout {
            let invalidated = self.state_timeout_tracker.invalidate_if_fresh(
                &key,
                event_order,
                armed_reset_at,
                armed_reset_version,
            );
            if invalidated && state_changed && pre_had_timeout {
                crate::runtime_metrics::record_state_timeout_cancelled(
                    ctx.tenant.as_str(),
                    ctx.entity_type,
                    &pre_state,
                );
            }
            return;
        }

        // A state change invalidates the prior timer and, when the destination
        // is timed, owns its replacement with the same generation. Advancing
        // once per durable response also rejects out-of-order callbacks.
        let mut transition_permit =
            if state_changed && !hydrating && (pre_had_timeout || post_has_timeout) {
                let Some(permit) = self.state_timeout_tracker.advance_if_fresh(
                    &key,
                    event_order,
                    armed_reset_at,
                    armed_reset_version,
                    post_timeout,
                ) else {
                    return;
                };
                if pre_had_timeout {
                    crate::runtime_metrics::record_state_timeout_cancelled(
                        ctx.tenant.as_str(),
                        ctx.entity_type,
                        &pre_state,
                    );
                }
                Some(permit)
            } else {
                None
            };

        // Arm timers for the matching destination declaration.
        for st in &state_timeouts {
            if st.state != post_state {
                continue;
            }
            let is_entry = state_changed && !hydrating;
            let is_reset =
                !hydrating && !state_changed && st.reset_on.iter().any(|a| a == ctx.action);
            let (permit, needs_hydration_rearm) = if is_entry {
                let Some(permit) = transition_permit.take() else {
                    continue;
                };
                (permit, false)
            } else if is_reset {
                let Some(permit) = self.state_timeout_tracker.advance_if_fresh(
                    &key,
                    event_order,
                    armed_reset_at,
                    armed_reset_version,
                    Some(st),
                ) else {
                    // Post-dispatch effects can finish out of order. An older
                    // reset must not supersede a newer durable response.
                    continue;
                };
                (permit, false)
            } else {
                // ADR-0056: reserve reconciliation ownership only when no
                // dispatch or hydration path has already armed a timer.
                let Some(permit) = self.state_timeout_tracker.reconcile_if_fresh(
                    &key,
                    event_order,
                    armed_reset_at,
                    armed_reset_version,
                    Some(st),
                ) else {
                    continue;
                };
                (permit, true)
            };
            let StateTimeoutPermit {
                generation: armed_seq,
                mut cancellation,
            } = permit;
            if is_reset {
                crate::runtime_metrics::record_state_timeout_reset(
                    ctx.tenant.as_str(),
                    ctx.entity_type,
                    &st.state,
                    ctx.action,
                );
            }

            // Determine the fire delay from the durable entry/reset anchor.
            //
            // Entry, reset, and hydration all share one absolute durable
            // deadline. This charges persistence and preceding post-dispatch
            // work instead of granting a fresh budget when arming runs late.
            let budget = Duration::from_secs(st.after_seconds);
            let now = match cause {
                StateTimeoutArmCause::PostDispatch => sim_now(),
                StateTimeoutArmCause::Hydration {
                    observed_at,
                    readiness_elapsed,
                } => hydration_reconciled_at(observed_at, readiness_elapsed),
            };
            let timeout = compute_timeout_delay(
                &response.state.events,
                response.state.state_timeout_clock_reset_at,
                &post_state,
                &st.reset_on,
                budget,
                now,
            );
            let delay = timeout.map_or(budget, |timeout| timeout.delay);
            if needs_hydration_rearm {
                if let Some(timeout) = timeout {
                    crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                        ctx.tenant.as_str(),
                        ctx.entity_type,
                        &st.state,
                        if timeout.overdue {
                            "overdue"
                        } else {
                            "budgeted"
                        },
                    );
                } else {
                    // No entry event found — treat as freshly entered.
                    // Safe default; worst case is one extra budget of wait.
                    crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                        ctx.tenant.as_str(),
                        ctx.entity_type,
                        &st.state,
                        "budgeted",
                    );
                }
            }

            self.state_timeout_tracker.inc_pending(ctx.entity_type);
            let params: serde_json::Value = serde_json::to_value(&st.params)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));

            let state = self.clone();
            let tracker = self.state_timeout_tracker.clone();
            let tenant = ctx.tenant.clone();
            let entity_type = ctx.entity_type.to_string();
            let entity_id = ctx.entity_id.to_string();
            let target_state = st.state.clone();
            let target_action = st.on_timeout.clone();
            let expected_timeout = st.clone();
            let mut table_versions_for_task = table_versions.clone();
            let mut agent_ctx = crate::request_context::AgentContext::for_service_inheriting(
                STATE_TIMEOUT_SERVICE,
                ctx.agent_ctx,
            );
            // The timer is a distinct internal dispatch with stable service
            // authority. Preserve only caller observability, replace the
            // initiating request key, and retain that replacement across every
            // delivery attempt.
            agent_ctx.idempotency_key = Some(format!(
                "state-timeout:{}:{}:{}:{}:{}",
                ctx.tenant,
                ctx.entity_type,
                ctx.entity_id,
                st.on_timeout,
                sim_uuid()
            ));
            let key_for_task = key.clone();
            let entity_type_for_dec = ctx.entity_type.to_string();
            let workflow_root_entity_type = agent_ctx
                .workflow_root_entity_type
                .clone()
                .unwrap_or_else(|| entity_type.clone());
            let workflow_root_entity_id = agent_ctx
                .workflow_root_entity_id
                .clone()
                .unwrap_or_else(|| entity_id.clone());
            let workflow_run_id = agent_ctx
                .workflow_run_id
                .clone()
                .unwrap_or_else(|| format!("{entity_type}:{entity_id}"));
            let deadline = timeout_deadline(delay); // determinism-ok: paused by DST

            tracing::debug!(
                tenant = %ctx.tenant,
                entity_type = ctx.entity_type,
                entity_id = ctx.entity_id,
                target_state = st.state.as_str(),
                target_action = st.on_timeout.as_str(),
                delay_ms = delay.as_millis() as u64,
                workflow.root_entity_type = %workflow_root_entity_type,
                workflow.root_entity_id = %workflow_root_entity_id,
                workflow.run_id = %workflow_run_id,
                "armed state timeout"
            );

            tokio::spawn(async move {
                // determinism-ok: wall-clock timer fires a side-effect action;
                // the action itself is deterministic under DST via sim_now().
                let deadline_reached = {
                    let watch = StateTimeoutWatch {
                        key: &key_for_task,
                        armed_generation: armed_seq,
                        tenant: &tenant,
                        entity_type: &entity_type,
                        entity_id: &entity_id,
                        target_state: &target_state,
                        expected_timeout: &expected_timeout,
                        agent_ctx: &agent_ctx,
                    };
                    state
                        .wait_for_state_timeout_deadline(
                            table_versions_for_task.as_mut(),
                            &mut cancellation,
                            deadline,
                            &watch,
                        )
                        .await
                };
                if !deadline_reached {
                    tracker.dec_pending(&entity_type_for_dec);
                    return;
                }

                let span = tracing::info_span!(
                    "dispatch.state_timeout.fire",
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    target_state = %target_state,
                    target_action = %target_action,
                    workflow.root_entity_type = %workflow_root_entity_type,
                    workflow.root_entity_id = %workflow_root_entity_id,
                    workflow.run_id = %workflow_run_id,
                );

                async move {
                    // Generation cancellation check. A newer accepted durable
                    // response renders this timer a no-op.
                    if *cancellation.borrow()
                        || tracker.current_generation(&key_for_task) != armed_seq
                    {
                        tracker.dec_pending(&entity_type_for_dec);
                        return;
                    }

                    let mut failure_count = 0_u32;
                    loop {
                        if *cancellation.borrow()
                            || tracker.current_generation(&key_for_task) != armed_seq
                        {
                            break;
                        }

                        let result = state
                            .dispatch_state_timeout_action(
                                DispatchCommand {
                                    tenant: &tenant,
                                    entity_type: &entity_type,
                                    entity_id: &entity_id,
                                    action: &target_action,
                                    params: params.clone(),
                                    agent_ctx: &agent_ctx,
                                    await_integration: false,
                                    await_reactions: true,
                                },
                                StateTimeoutPrecondition {
                                    expected_timeout: expected_timeout.clone(),
                                    expected_state: target_state.clone(),
                                    expected_reset_at: armed_reset_at,
                                    expected_reset_version: armed_reset_version,
                                },
                            )
                            .await;

                        let retry_after_ms = match result {
                            Ok(ref response)
                                if response.error.as_deref()
                                    == Some(STATE_TIMEOUT_PRECONDITION_MISMATCH) =>
                            {
                                break;
                            }
                            Ok(ref response) if response.success => {
                                crate::runtime_metrics::record_state_timeout_fired(
                                    tenant.as_str(),
                                    &entity_type,
                                    &target_state,
                                    &target_action,
                                );
                                break;
                            }
                            Ok(response) => {
                                failure_count = failure_count.saturating_add(1);
                                let delay = state_timeout_retry_delay(failure_count, None);
                                tracing::warn!(
                                    tenant = %tenant,
                                    entity_type,
                                    entity_id,
                                    target_state,
                                    target_action,
                                    failure_count,
                                    retry_delay_ms = u64::try_from(delay.as_millis())
                                        .unwrap_or(u64::MAX),
                                    error = response.error.as_deref().unwrap_or("unsuccessful response"),
                                    "state timeout delivery returned unsuccessfully; retaining ownership and retrying"
                                );
                                None
                            }
                            Err(error) => {
                                failure_count = failure_count.saturating_add(1);
                                let retry_after_ms = match &error {
                                    DispatchError::Deferred { retry_after_ms } => {
                                        Some(*retry_after_ms)
                                    }
                                    _ => None,
                                };
                                let delay =
                                    state_timeout_retry_delay(failure_count, retry_after_ms);
                                tracing::warn!(
                                    tenant = %tenant,
                                    entity_type,
                                    entity_id,
                                    target_state,
                                    target_action,
                                    failure_count,
                                    retry_delay_ms = u64::try_from(delay.as_millis())
                                        .unwrap_or(u64::MAX),
                                    error = %error,
                                    "state timeout delivery failed; retaining ownership and retrying"
                                );
                                retry_after_ms
                            }
                        };

                        if *cancellation.borrow()
                            || tracker.current_generation(&key_for_task) != armed_seq
                        {
                            break;
                        }
                        let retry_delay =
                            state_timeout_retry_delay(failure_count, retry_after_ms);
                        let retry_deadline_reached = {
                            let watch = StateTimeoutWatch {
                                key: &key_for_task,
                                armed_generation: armed_seq,
                                tenant: &tenant,
                                entity_type: &entity_type,
                                entity_id: &entity_id,
                                target_state: &target_state,
                                expected_timeout: &expected_timeout,
                                agent_ctx: &agent_ctx,
                            };
                            state
                                .wait_for_state_timeout_deadline(
                                    table_versions_for_task.as_mut(),
                                    &mut cancellation,
                                    timeout_deadline(retry_delay),
                                    &watch,
                                )
                                .await
                        };
                        if !retry_deadline_reached {
                            break;
                        }
                    }
                    tracker.dec_pending(&entity_type_for_dec);
                }
                .instrument(span)
                .await;
            });
        }
    }
}
