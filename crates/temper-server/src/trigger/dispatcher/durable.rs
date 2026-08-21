//! Durable reaction recovery, leasing, retry, and reconciliation.

mod helpers;

use crate::request_context::AgentContext;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use super::super::types::{MAX_REACTION_DEPTH, ReactionResult, ReactionRule};
use super::{BoundDelivery, ReactionDispatcher};
use helpers::{
    automatic_retry_backoff, is_expected_target_drop, is_transient_delivery_error,
    record_delivery_terminal_metrics,
};

impl ReactionDispatcher {
    /// Scan committed journals and deliver non-terminal intents within a
    /// caller-supplied inspection budget. This is the restart recovery entry point.
    pub async fn recover_tenant_deliveries(
        &self,
        state: &crate::ServerState,
        tenant: &TenantId,
        work_budget: usize,
    ) -> Result<usize, String> {
        use crate::trigger::delivery::{
            ReactionDeliveryStatus, extract_intents, load_delivery_record,
        };

        if work_budget == 0 {
            return Ok(0);
        }
        let recovery_lock = self.recovery_lock(tenant);
        let _recovery_guard = recovery_lock.lock().await;
        let (store, _) = state
            .event_journal()
            .ok_or_else(|| "durable reaction recovery requires an event journal".to_string())?;
        let mut cursor = self.recovery_cursor(tenant);
        if cursor.after_journal.is_none()
            && cursor.current_journal.is_none()
            && cursor.queued_journals.is_empty()
            && cursor.event_sequence == 0
            && cursor.intent_offset == 0
        {
            cursor.next_wakeup = None;
        }
        let mut inspected = 0usize;
        let mut recovered = 0usize;
        while inspected < work_budget {
            if cursor.current_journal.is_none() {
                if cursor.queued_journals.is_empty() {
                    cursor.queued_journals = store
                        .list_journal_ids_page(
                            tenant.as_str(),
                            None,
                            cursor
                                .after_journal
                                .as_ref()
                                .map(|(entity_type, entity_id)| {
                                    (entity_type.as_str(), entity_id.as_str())
                                }),
                            256,
                        )
                        .await
                        .map_err(|error| error.to_string())?
                        .into();
                }
                let Some(next) = cursor.queued_journals.pop_front() else {
                    let next_wakeup = cursor.next_wakeup;
                    cursor = super::RecoveryCursor {
                        next_wakeup,
                        ..super::RecoveryCursor::default()
                    };
                    self.set_recovery_cursor(tenant, cursor);
                    return Ok(recovered);
                };
                cursor.current_journal = Some(next);
            }
            let (entity_type, entity_id) = cursor
                .current_journal
                .clone()
                .expect("recovery selected a current journal");
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let events = store
                .read_events_limited(&persistence_id, cursor.event_sequence, 1)
                .await
                .map_err(|error| error.to_string())?;
            let Some(event) = events.into_iter().next() else {
                cursor.after_journal = cursor.current_journal.take();
                cursor.event_sequence = 0;
                cursor.intent_offset = 0;
                inspected = inspected.saturating_add(1);
                continue;
            };
            let intents = extract_intents(&event.payload).map_err(|error| error.to_string())?;
            if cursor.intent_offset >= intents.len() {
                cursor.event_sequence = event.sequence_nr;
                cursor.intent_offset = 0;
                inspected = inspected.saturating_add(1);
                continue;
            }
            let intent = intents[cursor.intent_offset].clone();
            cursor.intent_offset = cursor.intent_offset.saturating_add(1);
            inspected = inspected.saturating_add(1);
            let (record, _) = load_delivery_record(&store, intent.clone())
                .await
                .map_err(|error| error.to_string())?;
            if matches!(
                record.status,
                ReactionDeliveryStatus::Succeeded
                    | ReactionDeliveryStatus::Skipped
                    | ReactionDeliveryStatus::DroppedAllowed
                    | ReactionDeliveryStatus::Rejected
                    | ReactionDeliveryStatus::DeadLettered
            ) {
                continue;
            }
            let now = temper_runtime::scheduler::sim_now();
            let future_wakeup = match record.status {
                ReactionDeliveryStatus::Pending => {
                    record.next_attempt_at.filter(|next| *next > now)
                }
                ReactionDeliveryStatus::Claimed | ReactionDeliveryStatus::Dispatching => {
                    record.lease_expires_at.filter(|expiry| *expiry > now)
                }
                ReactionDeliveryStatus::Succeeded
                | ReactionDeliveryStatus::Skipped
                | ReactionDeliveryStatus::DroppedAllowed
                | ReactionDeliveryStatus::Rejected
                | ReactionDeliveryStatus::DeadLettered => None,
            };
            if let Some(next_wakeup) = future_wakeup {
                cursor.next_wakeup = Some(
                    cursor
                        .next_wakeup
                        .map_or(next_wakeup, |current| current.min(next_wakeup)),
                );
                continue;
            }
            match self.dispatch_committed_intent(state, intent).await {
                Ok(_) => recovered = recovered.saturating_add(1),
                Err(error) if error == "reaction delivery is already leased" => {}
                Err(error) => return Err(error),
            }
        }
        self.set_recovery_cursor(tenant, cursor);
        Ok(recovered)
    }

    /// Drain due work and deterministic retry backoff for one tenant within a
    /// caller-owned wall-time budget. Durable state remains the source of
    /// truth when the budget expires; a later worker resumes it.
    pub async fn drain_tenant_deliveries(
        &self,
        state: &crate::ServerState,
        tenant: &TenantId,
        work_budget: usize,
        max_wait: std::time::Duration,
    ) -> Result<usize, String> {
        let deadline = tokio::time::Instant::now() + max_wait; // determinism-ok: caller wall-time budget, not persisted ordering
        let mut total = 0usize;
        loop {
            let now = tokio::time::Instant::now(); // determinism-ok: caller wall-time budget, not persisted ordering
            if now >= deadline {
                return Ok(total);
            }
            let recovered = match tokio::time::timeout(
                deadline - now,
                self.recover_tenant_deliveries(state, tenant, work_budget),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => return Ok(total),
            };
            total = total.saturating_add(recovered);
            let now = tokio::time::Instant::now(); // determinism-ok: caller wall-time budget, not persisted ordering
            if now >= deadline {
                return Ok(total);
            }
            let delay = if self.recovery_scan_in_progress(tenant) {
                std::time::Duration::ZERO
            } else if let Some(delay) = self.next_tenant_delivery_delay(tenant) {
                delay
            } else {
                return Ok(total);
            }
            .min(deadline - now);
            tokio::time::sleep(delay).await; // determinism-ok: production poll cadence; persisted scheduler timestamps determine eligibility
        }
    }

    fn next_tenant_delivery_delay(&self, tenant: &TenantId) -> Option<std::time::Duration> {
        let now = temper_runtime::scheduler::sim_now();
        self.recovery_cursor(tenant)
            .next_wakeup
            .map(|next| next.signed_duration_since(now).to_std().unwrap_or_default())
    }

    /// Deliver one intent read from a committed source event.
    ///
    /// Every lifecycle mutation is appended under optimistic concurrency. A
    /// competing or stale worker therefore cannot advance an older fence.
    pub async fn dispatch_committed_intent(
        &self,
        state: &crate::ServerState,
        intent: crate::trigger::delivery::PersistedReactionIntent,
    ) -> Result<Vec<ReactionResult>, String> {
        use crate::trigger::delivery::{
            MAX_AUTOMATIC_ATTEMPTS, ReactionDeliveryStatus, append_delivery_record,
            load_delivery_record,
        };

        if let Some(pin) = intent.schema_pin.as_ref() {
            crate::schema_deployment::GovernedSchemaDeploymentService::new(state)
                .recover_registry_bundle(
                    &intent.tenant,
                    &pin.execution.scope,
                    &pin.execution.bundle_digest,
                )
                .await
                .map_err(|error| error.message().to_string())?;
        }

        let (store, _) = state
            .event_journal()
            .ok_or_else(|| "durable reaction delivery requires an event journal".to_string())?;
        let (mut record, mut sequence) = load_delivery_record(&store, intent.clone())
            .await
            .map_err(|error| error.to_string())?;
        if sequence == 0 {
            crate::runtime_metrics::record_reaction_delivery_event("queued");
        }
        if matches!(
            record.status,
            ReactionDeliveryStatus::Succeeded
                | ReactionDeliveryStatus::Skipped
                | ReactionDeliveryStatus::DroppedAllowed
                | ReactionDeliveryStatus::Rejected
                | ReactionDeliveryStatus::DeadLettered
        ) {
            return Ok(Vec::new());
        }

        let now = temper_runtime::scheduler::sim_now();
        if matches!(
            record.status,
            ReactionDeliveryStatus::Claimed | ReactionDeliveryStatus::Dispatching
        ) {
            if !record.recover_expired_lease(now) {
                return Err("reaction delivery is already leased".to_string());
            }
            crate::runtime_metrics::record_reaction_delivery_lease_recovered();
            sequence = append_delivery_record(&store, sequence, &record)
                .await
                .map_err(|error| error.to_string())?;
        }

        let rule: ReactionRule = match serde_json::from_value(intent.rule.clone()) {
            Ok(rule) => rule,
            Err(error) => {
                record.status = ReactionDeliveryStatus::Rejected;
                record.last_error = Some(format!("invalid persisted reaction rule: {error}"));
                append_delivery_record(&store, sequence, &record)
                    .await
                    .map_err(|append_error| append_error.to_string())?;
                record_delivery_terminal_metrics(&record);
                return Ok(Vec::new());
            }
        };
        if rule
            .when
            .to_state
            .as_deref()
            .is_some_and(|expected| expected != intent.source_to_state)
        {
            record.status = ReactionDeliveryStatus::Skipped;
            append_delivery_record(&store, sequence, &record)
                .await
                .map_err(|error| error.to_string())?;
            record_delivery_terminal_metrics(&record);
            return Ok(Vec::new());
        }
        if !intent.guard_passed {
            record.status = ReactionDeliveryStatus::Skipped;
            append_delivery_record(&store, sequence, &record)
                .await
                .map_err(|error| error.to_string())?;
            record_delivery_terminal_metrics(&record);
            return Ok(Vec::new());
        }
        if intent.depth >= MAX_REACTION_DEPTH {
            record.status = ReactionDeliveryStatus::Rejected;
            record.last_error = Some("reaction cascade depth budget exhausted".to_string());
            append_delivery_record(&store, sequence, &record)
                .await
                .map_err(|error| error.to_string())?;
            record_delivery_terminal_metrics(&record);
            return Ok(Vec::new());
        }

        if let Some(target_entity_id) = intent.target_entity_id.as_deref() {
            let target_persistence_id = match intent.schema_pin.as_ref() {
                Some(pin) => format!(
                    "{}:{}:{}",
                    intent.tenant,
                    rule.then.entity_type,
                    temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                        target_entity_id,
                        &pin.execution,
                    )
                ),
                None => format!(
                    "{}:{}:{}",
                    intent.tenant, rule.then.entity_type, target_entity_id
                ),
            };
            let target_events = store
                .read_latest_events(
                    &target_persistence_id,
                    crate::entity_actor::types::MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY,
                )
                .await
                .map_err(|error| error.to_string())?;
            let matching_receipt = target_events.iter().find(|event| {
                crate::trigger::delivery::extract_receipt(&event.payload)
                    .ok()
                    .flatten()
                    .is_some_and(|receipt| receipt.delivery_id == intent.delivery_id)
            });
            if let Some(target_event) = matching_receipt {
                let tenant = TenantId::new(&intent.tenant);
                let descendants = state
                    .materialize_committed_reaction_intents(
                        &tenant,
                        &rule.then.entity_type,
                        target_entity_id,
                        target_event.sequence_nr,
                        intent.schema_pin.as_ref().map(|pin| &pin.execution),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                if !descendants.is_empty() {
                    self.notify_recovery(&tenant);
                }
                crate::runtime_metrics::record_reaction_delivery_event("reconciled");
                record.status = ReactionDeliveryStatus::Succeeded;
                record.lease_expires_at = None;
                record.last_error = None;
                append_delivery_record(&store, sequence, &record)
                    .await
                    .map_err(|error| error.to_string())?;
                record_delivery_terminal_metrics(&record);
                return Ok(Vec::new());
            }
        }

        let security_ctx: SecurityContext = match serde_json::from_value(intent.authority.clone()) {
            Ok(context) => context,
            Err(error) => {
                record.status = ReactionDeliveryStatus::Rejected;
                record.last_error = Some(format!("invalid persisted reaction authority: {error}"));
                append_delivery_record(&store, sequence, &record)
                    .await
                    .map_err(|append_error| append_error.to_string())?;
                record_delivery_terminal_metrics(&record);
                return Ok(Vec::new());
            }
        };
        let fencing_token = record
            .claim(now, chrono::Duration::seconds(30))
            .map_err(|error| error.to_string())?;
        sequence = append_delivery_record(&store, sequence, &record)
            .await
            .map_err(|error| error.to_string())?;
        crate::runtime_metrics::record_reaction_delivery_event("claimed");
        record
            .begin_dispatch(fencing_token)
            .map_err(|error| error.to_string())?;
        sequence = append_delivery_record(&store, sequence, &record)
            .await
            .map_err(|error| error.to_string())?;

        let invoking_ctx = AgentContext {
            security_ctx: Some(security_ctx),
            idempotency_key: Some(intent.delivery_id.clone()),
            schema_pin: intent.schema_pin.as_ref().map(|pin| pin.execution.clone()),
            ..AgentContext::default()
        };
        let drop_ok = rule.drop_ok;
        let results = self
            .dispatch_rules(
                state,
                &TenantId::new(&intent.tenant),
                &intent.source_entity_type,
                &intent.source_entity_id,
                &intent.source_action,
                &intent.source_to_state,
                &intent.source_fields,
                intent.depth,
                &invoking_ctx,
                vec![rule],
                Some(BoundDelivery {
                    delivery_id: intent.delivery_id.clone(),
                    root_delivery_id: intent.root_delivery_id.clone(),
                    fencing_token,
                    target_entity_id: intent.target_entity_id.clone(),
                }),
            )
            .await;

        record.lease_expires_at = None;
        record.next_attempt_at = None;
        if results.iter().any(|result| result.success) {
            record.status = ReactionDeliveryStatus::Succeeded;
            record.last_error = None;
        } else if results.is_empty() {
            record.status = ReactionDeliveryStatus::Skipped;
            record.last_error = None;
        } else {
            let error = results
                .iter()
                .find_map(|result| result.error.clone())
                .unwrap_or_else(|| "reaction target rejected the action".to_string());
            let transient = is_transient_delivery_error(&error);
            let dropped_allowed = drop_ok && is_expected_target_drop(&error);
            record.transient_failure = transient;
            record.last_error = Some(error);
            record.status = if transient && record.attempts < MAX_AUTOMATIC_ATTEMPTS {
                crate::runtime_metrics::record_reaction_delivery_event("automatic_retry_scheduled");
                record.next_attempt_at = Some(
                    temper_runtime::scheduler::sim_now() + automatic_retry_backoff(record.attempts),
                );
                ReactionDeliveryStatus::Pending
            } else if transient {
                ReactionDeliveryStatus::DeadLettered
            } else if dropped_allowed {
                ReactionDeliveryStatus::DroppedAllowed
            } else {
                ReactionDeliveryStatus::Rejected
            };
        }
        append_delivery_record(&store, sequence, &record)
            .await
            .map_err(|error| error.to_string())?;
        record_delivery_terminal_metrics(&record);
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::helpers::{is_expected_target_drop, is_transient_delivery_error};

    #[test]
    fn source_snapshot_races_are_retried() {
        assert!(is_transient_delivery_error("SequenceConflict"));
    }

    #[test]
    fn drop_ok_only_classifies_target_state_mismatch() {
        assert!(is_expected_target_drop(
            "Action 'Capture' not valid from state 'Pending'"
        ));
        assert!(is_expected_target_drop(
            "Action 'Capture' blocked from state 'Pending': guard failed"
        ));
        assert!(!is_expected_target_drop("authorization denied"));
        assert!(!is_expected_target_drop("invalid persisted authority"));
    }
}
