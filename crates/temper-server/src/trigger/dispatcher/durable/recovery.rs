//! Bounded tenant delivery scanning and supervisor draining.

use temper_runtime::tenant::TenantId;

use super::super::ReactionDispatcher;

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
                    cursor = super::super::RecoveryCursor {
                        next_wakeup,
                        ..super::super::RecoveryCursor::default()
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
}
