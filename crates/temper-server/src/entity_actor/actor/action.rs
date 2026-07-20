//! Spec-governed entity action execution.

use super::*;

mod admission;
mod finalization;

impl EntityActor {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_action(
        &self,
        name: String,
        params: serde_json::Value,
        cross_entity_booleans: BTreeMap<String, bool>,
        idempotency_key: Option<String>,
        state_timeout_precondition: Option<Box<StateTimeoutPrecondition>>,
        state: &mut EntityState,
        ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        // Capture start time for span duration (DST-safe: sim_now()
        // returns logical clock in simulation, wall clock in production).
        let action_start = sim_now();
        // Wall-clock start for `temper_actor_ask_reply_latency_ms`.
        // Separate from `action_start` because metrics emission is
        // outside the DST boundary; using Instant here is safe.
        let ask_reply_start = Instant::now(); // determinism-ok: observability only

        // Snapshot the current table for this action dispatch.
        // On the next action, any hot-swapped table will be picked up.
        let table = self.table.read().expect("table lock poisoned").clone();

        let actor_key = self.persistence_id();
        if self.reply_to_short_circuited_action(
            &table,
            &actor_key,
            &name,
            &cross_entity_booleans,
            idempotency_key.as_deref(),
            state,
            state_timeout_precondition.as_deref(),
            ctx,
        ) {
            return Ok(());
        }

        // Captured BEFORE the action applies. The retry path (ADR-0046)
        // updates these in lockstep with replay so postconditions hold
        // across the race window.
        let mut event_count_before = state.total_event_count;
        let mut state_before = state.clone();
        let field_sync_mode =
            Self::field_sync_mode_for_backend(self.event_backend, self.blob_store.as_ref());

        // `result` and `event` are `mut` so that a successful ADR-0046
        // retry can replace them with values re-evaluated against the
        // caught-up state. The downstream telemetry and reply use
        // whichever pair last succeeded in persist.
        let mut committed_table = table.clone();
        let mut result = process_action_with_xref_and_field_mode(
            state,
            &table,
            &name,
            &params,
            &cross_entity_booleans,
            field_sync_mode,
        );

        if result.success {
            // process_action returned a successful transition with event.
            // Clone out so `result.event` stays populated for re-use if
            // the retry path needs to re-emit (simplifies lifetime here).
            let mut event = result
                .event
                .clone()
                .expect("successful process_action always returns event"); // ci-ok: post-assertion, success guarantees Some
            event.idempotency_key = idempotency_key.clone();
            let mut persisted_timeout_clock = None;

            if !result.overflow_blobs.is_empty()
                && let Err(e) =
                    Self::persist_overflow_blobs(self.blob_store.as_ref(), &result.overflow_blobs)
                        .await
            {
                *state = state_before;
                ctx.reply(EntityResponse {
                    success: false,
                    state: state.clone(),
                    error: Some(format!("field-overflow blob persistence failed: {e}")),
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
                return Ok(());
            }

            // Persist to Postgres (if configured). On
            // `ConcurrencyViolation` enter the ADR-0046 retry cycle —
            // replay events, re-evaluate the action against the caught-up
            // state, and retry the persist up to two more times. Other
            // error variants fail immediately (same as before).
            if let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend)
            {
                let first_persist = self
                    .persist_event(
                        store,
                        backend,
                        &self.persistence_id(),
                        &table,
                        state,
                        &event,
                    )
                    .await;

                match first_persist {
                    Ok((_, clock)) => {
                        persisted_timeout_clock = Some(clock);
                        // Happy path — fall through to downstream telemetry.
                    }
                    Err(PersistenceError::ConcurrencyViolation {
                        expected: _,
                        actual,
                    }) => {
                        // ADR-0046 Sub-Decision 3: dedicated APM span
                        // covering the retry cycle. `attempts` and
                        // `outcome` are recorded at the end so Datadog
                        // APM can filter and chart conflict-handling
                        // activity per entity type.
                        let retry_span = tracing::info_span!(
                            "temper.entity.persist_with_retry",
                            "entity.type" = %self.entity_type,
                            "entity.id" = %state.entity_id,
                            action = %name,
                            initial_actual = actual,
                            attempts = tracing::field::Empty,
                            outcome = tracing::field::Empty,
                        );

                        tracing::warn!(
                            parent: &retry_span,
                            entity = %state.entity_id,
                            action = %name,
                            actual_seq = actual,
                            "persist hit optimistic-concurrency violation; entering ADR-0046 retry"
                        );

                        // 2 retries + 1 initial = 3 total attempts (ADR-0046).
                        const MAX_RETRIES: u32 = 2;
                        let mut retry_idx: u32 = 0;
                        let mut retry_final: Option<(
                            crate::runtime_metrics::ConcurrencyRetryOutcome,
                            Option<String>,
                        )> = None;
                        // ADR-0046 Sub-Decision 4: track the most
                        // recent authoritative sequence across retries
                        // so the post-replay assertion catches a
                        // divergent replay even on a multi-conflict
                        // cycle. Seeded from the initial violation;
                        // refreshed from each subsequent violation.
                        let mut last_actual: u64 = actual;

                        while retry_idx < MAX_RETRIES {
                            retry_idx += 1;

                            // One live table snapshot governs the
                            // complete retry attempt: authoritative
                            // replay, exact timeout revalidation,
                            // transition evaluation, and the eventual
                            // timeout-clock update. Mixing the original
                            // action table with a post-swap retry table
                            // can produce state that a fresh replay
                            // cannot reproduce.
                            let retry_table =
                                self.table.read().expect("table lock poisoned").clone();

                            // Rollback speculative state.
                            *state = state_before.clone();

                            // Catch up to the authoritative sequence.
                            Self::replay_events(
                                &retry_table,
                                store,
                                backend,
                                state,
                                &self.tenant,
                                self.blob_store.as_ref(),
                                // A retry must not re-run the action from
                                // state that may be missing a committed tail.
                                true,
                            )
                            .await?;

                            // ADR-0046 Sub-Decision 4: replay must at
                            // minimum reach the sequence the store
                            // reported. Reaching further is fine (a
                            // later writer may have appended during
                            // our own round trip).
                            debug_assert!(
                                state.sequence_nr >= last_actual,
                                "POSTCONDITION: replay under-reached authoritative sequence \
                                         (state.sequence_nr={} < last_actual={last_actual})",
                                state.sequence_nr
                            );

                            // Refresh baselines so postconditions hold
                            // against the replayed state, not the
                            // pre-race snapshot.
                            state_before = state.clone();
                            event_count_before = state.total_event_count;

                            // The initial condition and transition
                            // evaluation happened against speculative
                            // state that just lost an optimistic race.
                            // A competing replica may have committed a
                            // reset or a hot-swap may have replaced the
                            // declaration in that gap. Revalidate both
                            // authoritative state and the exact live
                            // declaration before evaluating or
                            // persisting another attempt.
                            if state_timeout_precondition_is_stale(
                                &retry_table,
                                state,
                                state_timeout_precondition.as_deref(),
                            ) {
                                retry_final = Some((
                                    crate::runtime_metrics::ConcurrencyRetryOutcome::ActionIllegal,
                                    Some(STATE_TIMEOUT_PRECONDITION_MISMATCH.to_string()),
                                ));
                                break;
                            }

                            // Re-evaluate the action against the caught-up
                            // state. It may now fail (entity reached a
                            // terminal state during the race) — if so,
                            // surface that error rather than silently
                            // dropping the caller.
                            let retry_result = process_action_with_xref_and_field_mode(
                                state,
                                &retry_table,
                                &name,
                                &params,
                                &cross_entity_booleans,
                                field_sync_mode,
                            );

                            if !retry_result.success {
                                retry_final = Some((
                                    crate::runtime_metrics::ConcurrencyRetryOutcome::ActionIllegal,
                                    Some(retry_result.error.unwrap_or_else(|| {
                                        format!(
                                            "action {name} no longer legal after concurrency replay"
                                        )
                                    })),
                                ));
                                break;
                            }

                            let retry_event = retry_result
                                .event
                                .clone()
                                .expect("successful process_action always returns event"); // ci-ok: post-assertion, success guarantees Some
                            let mut retry_event = retry_event;
                            retry_event.idempotency_key = idempotency_key.clone();

                            // Overflow blobs for the re-evaluated result.
                            if !retry_result.overflow_blobs.is_empty()
                                && let Err(e) = Self::persist_overflow_blobs(
                                    self.blob_store.as_ref(),
                                    &retry_result.overflow_blobs,
                                )
                                .await
                            {
                                retry_final = Some((
                                    crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                    Some(format!(
                                        "field-overflow blob persistence failed during retry: {e}"
                                    )),
                                ));
                                break;
                            }

                            // Backoff: retry 1 → 10ms, retry 2 → 50ms.
                            let backoff_ms = if retry_idx == 1 { 10 } else { 50 };
                            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await; // determinism-ok: rare retry backoff (ADR-0046)

                            match self
                                .persist_event(
                                    store,
                                    backend,
                                    &self.persistence_id(),
                                    &retry_table,
                                    state,
                                    &retry_event,
                                )
                                .await
                            {
                                Ok((_, clock)) => {
                                    // Commit re-evaluated event + result into
                                    // downstream telemetry and reply.
                                    event = retry_event;
                                    result = retry_result;
                                    committed_table = retry_table;
                                    persisted_timeout_clock = Some(clock);
                                    retry_final = Some((
                                        crate::runtime_metrics::ConcurrencyRetryOutcome::Success,
                                        None,
                                    ));
                                    break;
                                }
                                Err(PersistenceError::ConcurrencyViolation {
                                    actual: new_actual,
                                    ..
                                }) if retry_idx < MAX_RETRIES => {
                                    // Capture the fresh authoritative
                                    // sequence so the next iteration's
                                    // post-replay assertion checks
                                    // against the right target.
                                    last_actual = new_actual;
                                    tracing::warn!(
                                        parent: &retry_span,
                                        entity = %state.entity_id,
                                        action = %name,
                                        attempt = retry_idx + 1,
                                        actual_seq = new_actual,
                                        "retry persist hit another concurrency violation; retrying"
                                    );
                                    continue;
                                }
                                Err(PersistenceError::ConcurrencyViolation { .. }) => {
                                    retry_final = Some((
                                                crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                                Some(
                                                    "persistence failed: optimistic concurrency retry exhausted"
                                                        .to_string(),
                                                ),
                                            ));
                                    break;
                                }
                                Err(e) => {
                                    retry_final = Some((
                                        crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                        Some(format!("persistence failed during retry: {e}")),
                                    ));
                                    break;
                                }
                            }
                        }

                        // Record the retry outcome. `total_attempts` is
                        // 1-based; `retry_idx` counts completed retries.
                        let total_attempts = u64::from(1 + retry_idx);
                        if let Some((outcome, err_msg)) = retry_final {
                            // Close the ADR-0046 APM span with the
                            // final attempt count + outcome so APM
                            // views can filter by either.
                            retry_span.record("attempts", total_attempts);
                            retry_span.record("outcome", outcome.as_str());
                            crate::runtime_metrics::record_entity_concurrency_retry(
                                &self.entity_type,
                                outcome,
                                total_attempts,
                            );
                            if let Some(msg) = err_msg {
                                *state = state_before;
                                ctx.reply(EntityResponse {
                                    success: false,
                                    state: state.clone(),
                                    error: Some(msg),
                                    custom_effects: vec![],
                                    scheduled_actions: vec![],
                                    spawn_requests: vec![],
                                    spec_governed: true,
                                });
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => {
                        // Non-concurrency persistence error — unchanged:
                        // roll back and fail.
                        *state = state_before;
                        ctx.reply(EntityResponse {
                            success: false,
                            state: state.clone(),
                            error: Some(format!("persistence failed: {e}")),
                            custom_effects: vec![],
                            scheduled_actions: vec![],
                            spawn_requests: vec![],
                            spec_governed: true,
                        });
                        return Ok(());
                    }
                }
            }

            let action_end = sim_now();
            let duration_ns = (action_end - action_start)
                .num_nanoseconds()
                .unwrap_or(0)
                .max(0) as u64;
            self.finalize_successful_action(
                duration_ns,
                &name,
                state,
                event,
                persisted_timeout_clock,
                &committed_table,
                event_count_before,
                result,
                idempotency_key.as_deref(),
                &actor_key,
                ctx,
            )
            .await;
        } else {
            // Transition failed — emit telemetry
            let action_end = sim_now();
            let duration_ns = (action_end - action_start)
                .num_nanoseconds()
                .unwrap_or(0)
                .max(0) as u64;
            let wide = wide_event::from_transition(wide_event::TransitionInput {
                tenant: &self.tenant,
                entity_type: &state.entity_type,
                entity_id: &state.entity_id,
                operation: &name,
                from_status: &state.status,
                to_status: &state.status,
                success: false,
                duration_ns,
                params: &params,
                item_count: state.item_count,
                trace_id: &self.trace_id,
            });
            wide_event::emit_span(&wide);
            wide_event::emit_metrics(&wide);

            ctx.reply(EntityResponse {
                success: false,
                state: state.clone(),
                error: result.error,
                custom_effects: vec![],
                scheduled_actions: vec![],
                spawn_requests: vec![],
                spec_governed: true,
            });
        }
        // Inside-actor ask reply latency (excludes dispatch and retry
        // overhead). Early-exit error paths above `return Ok(())` are
        // not counted; the signal of interest is normal action
        // handling latency.
        crate::runtime_metrics::record_actor_ask_reply_latency(
            &state.entity_type,
            &name,
            ask_reply_start.elapsed(),
        );
        Ok(())
    }
}
