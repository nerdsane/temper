//! Durable reaction recovery, leasing, retry, and reconciliation.

mod collection;
mod helpers;
mod recovery;
mod state_timeout;

use crate::request_context::AgentContext;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use super::super::types::{MAX_REACTION_DEPTH, ReactionResult, ReactionRule};
use super::{BoundDelivery, ReactionDispatcher};
use helpers::{
    automatic_retry_backoff, collection_control_skip_reason, is_expected_target_drop,
    is_transient_delivery_error, record_delivery_terminal_metrics,
};
use state_timeout::validate_timeout_clock;

enum TimeoutClockStatus {
    Current(u64),
    Superseded(String),
    Rejected(String),
}

async fn persist_terminal_delivery(
    store: &crate::storage::BoxedEventStore,
    sequence: u64,
    record: &crate::trigger::delivery::ReactionDeliveryRecord,
) -> Result<(), String> {
    if !crate::trigger::collection_workflow::commit_terminal_delivery(store, sequence, record)
        .await?
    {
        crate::trigger::delivery::append_delivery_record(store, sequence, record)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl ReactionDispatcher {
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
            crate::runtime_metrics::record_reaction_delivery_event(
                intent.kind.metric_label(),
                "queued",
            );
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
                // Another fenced owner is making progress. Duplicate wakeups
                // are successful no-ops; restart recovery will reconcile its
                // receipt or reclaim the lease after expiry.
                return Ok(Vec::new());
            }
            crate::runtime_metrics::record_reaction_delivery_lease_recovered(
                intent.kind.metric_label(),
            );
            if let Some(collection) = intent.collection.as_ref() {
                let role = crate::runtime_metrics::collection_delivery_role_label(collection.role);
                crate::runtime_metrics::record_collection_workflow_event("lease_recovered", role);
            }
            sequence = match append_delivery_record(&store, sequence, &record).await {
                Ok(sequence) => sequence,
                Err(temper_runtime::persistence::PersistenceError::ConcurrencyViolation {
                    ..
                }) => return Ok(Vec::new()),
                Err(error) => return Err(error.to_string()),
            };
        }
        if record
            .next_attempt_at
            .is_some_and(|eligible| eligible > now)
        {
            return Ok(Vec::new());
        }
        let automatic_attempt_budget = intent
            .collection
            .as_ref()
            .map_or(MAX_AUTOMATIC_ATTEMPTS, |context| {
                u32::from(context.max_attempts)
            });
        if record.attempts >= automatic_attempt_budget {
            record.status = ReactionDeliveryStatus::DeadLettered;
            record.transient_failure = true;
            record.lease_expires_at = None;
            record.next_attempt_at = None;
            record.last_error = Some("automatic delivery attempt budget exhausted".to_string());
            persist_terminal_delivery(&store, sequence, &record).await?;
            record_delivery_terminal_metrics(&record);
            return Ok(Vec::new());
        }
        let timeout_shape_matches_kind = matches!(
            (intent.kind, intent.state_timeout.is_some()),
            (crate::trigger::delivery::DeliveryKind::Reaction, false)
                | (crate::trigger::delivery::DeliveryKind::StateTimeout, true)
                | (
                    crate::trigger::delivery::DeliveryKind::CollectionMember,
                    false
                )
                | (
                    crate::trigger::delivery::DeliveryKind::CollectionCancellation,
                    false
                )
                | (
                    crate::trigger::delivery::DeliveryKind::CollectionJoin,
                    false
                )
        );
        if !timeout_shape_matches_kind {
            record.status = ReactionDeliveryStatus::Rejected;
            record.last_error = Some("delivery kind and timeout evidence disagree".to_string());
            persist_terminal_delivery(&store, sequence, &record).await?;
            record_delivery_terminal_metrics(&record);
            return Ok(Vec::new());
        }

        let rule: ReactionRule = match serde_json::from_value(intent.rule.clone()) {
            Ok(rule) => rule,
            Err(error) => {
                record.status = ReactionDeliveryStatus::Rejected;
                record.last_error = Some(format!("invalid persisted reaction rule: {error}"));
                persist_terminal_delivery(&store, sequence, &record).await?;
                record_delivery_terminal_metrics(&record);
                return Ok(Vec::new());
            }
        };
        if intent.kind == crate::trigger::delivery::DeliveryKind::CollectionCancellation {
            self.quiesce_controlled_member_descendants(state, &store, &intent)
                .await?;
        }
        if rule
            .when
            .to_state
            .as_deref()
            .is_some_and(|expected| expected != intent.source_to_state)
        {
            record.status = ReactionDeliveryStatus::Skipped;
            persist_terminal_delivery(&store, sequence, &record).await?;
            record_delivery_terminal_metrics(&record);
            return Ok(Vec::new());
        }
        if !intent.guard_passed {
            record.status = ReactionDeliveryStatus::Skipped;
            persist_terminal_delivery(&store, sequence, &record).await?;
            record_delivery_terminal_metrics(&record);
            return Ok(Vec::new());
        }
        if intent.depth >= MAX_REACTION_DEPTH {
            record.status = ReactionDeliveryStatus::Rejected;
            record.last_error = Some("reaction cascade depth budget exhausted".to_string());
            persist_terminal_delivery(&store, sequence, &record).await?;
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
                if intent.collection.is_some() {
                    self.dispatch_collection_descendants(state, descendants)
                        .await?;
                } else if !descendants.is_empty() {
                    self.notify_recovery(&tenant);
                }
                crate::runtime_metrics::record_reaction_delivery_event(
                    intent.kind.metric_label(),
                    "reconciled",
                );
                if let Some(collection) = intent.collection.as_ref() {
                    let role =
                        crate::runtime_metrics::collection_delivery_role_label(collection.role);
                    crate::runtime_metrics::record_collection_workflow_event(
                        "duplicate_receipt",
                        role,
                    );
                }
                record.status = ReactionDeliveryStatus::Succeeded;
                record.lease_expires_at = None;
                record.last_error = None;
                persist_terminal_delivery(&store, sequence, &record).await?;
                record_delivery_terminal_metrics(&record);
                return Ok(Vec::new());
            }
        }

        let mut expected_target_sequence = None;
        if intent.state_timeout.is_some() {
            match validate_timeout_clock(state, &store, &intent, &rule).await {
                TimeoutClockStatus::Current(sequence) => {
                    expected_target_sequence = Some(sequence);
                }
                TimeoutClockStatus::Superseded(reason) => {
                    record.status = ReactionDeliveryStatus::Skipped;
                    record.lease_expires_at = None;
                    record.next_attempt_at = None;
                    record.last_error = Some(reason);
                    persist_terminal_delivery(&store, sequence, &record).await?;
                    record_delivery_terminal_metrics(&record);
                    return Ok(Vec::new());
                }
                TimeoutClockStatus::Rejected(reason) => {
                    record.status = ReactionDeliveryStatus::Rejected;
                    record.lease_expires_at = None;
                    record.next_attempt_at = None;
                    record.last_error = Some(reason);
                    persist_terminal_delivery(&store, sequence, &record).await?;
                    record_delivery_terminal_metrics(&record);
                    return Ok(Vec::new());
                }
            }
        }

        let security_ctx: SecurityContext = match serde_json::from_value(intent.authority.clone()) {
            Ok(context) => context,
            Err(error) => {
                record.status = ReactionDeliveryStatus::Rejected;
                record.last_error = Some(format!("invalid persisted reaction authority: {error}"));
                persist_terminal_delivery(&store, sequence, &record).await?;
                record_delivery_terminal_metrics(&record);
                return Ok(Vec::new());
            }
        };
        let fencing_token = record
            .claim(now, chrono::Duration::seconds(30))
            .map_err(|error| error.to_string())?;
        sequence = match append_delivery_record(&store, sequence, &record).await {
            Ok(sequence) => sequence,
            Err(temper_runtime::persistence::PersistenceError::ConcurrencyViolation { .. }) => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.to_string()),
        };
        crate::runtime_metrics::record_reaction_delivery_event(
            intent.kind.metric_label(),
            if intent.not_before.is_some_and(|deadline| deadline < now) {
                "claimed_overdue"
            } else {
                "claimed"
            },
        );
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
                    expected_target_sequence,
                    state_timeout_state: intent
                        .state_timeout
                        .as_ref()
                        .map(|clock| clock.state.clone()),
                    collection: intent.collection.clone().map(|mut collection| {
                        collection.attempts = u8::try_from(record.attempts).unwrap_or(u8::MAX);
                        collection
                    }),
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
            let migrated_timeout = intent.state_timeout.is_some()
                && error.contains("migrated scoped schema write fence");
            let collection_control_skip = collection_control_skip_reason(
                intent.collection.as_ref().map(|context| context.role),
                &error,
            );
            let transient = is_transient_delivery_error(&error);
            let dropped_allowed = drop_ok && is_expected_target_drop(&error);
            record.transient_failure = transient;
            record.last_error = Some(error);
            record.status = if migrated_timeout || collection_control_skip.is_some() {
                ReactionDeliveryStatus::Skipped
            } else if transient && record.attempts < automatic_attempt_budget {
                crate::runtime_metrics::record_reaction_delivery_event(
                    intent.kind.metric_label(),
                    "automatic_retry_scheduled",
                );
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
            if let Some(reason) = collection_control_skip {
                record.last_error = Some(reason.to_string());
            }
        }
        if record.status.is_terminal() {
            persist_terminal_delivery(&store, sequence, &record).await?;
        } else {
            append_delivery_record(&store, sequence, &record)
                .await
                .map_err(|error| error.to_string())?;
        }
        record_delivery_terminal_metrics(&record);
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::helpers::{
        collection_control_skip_reason, is_expected_target_drop, is_transient_delivery_error,
    };

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

    #[test]
    fn post_control_descendant_fence_has_stable_skip_reason() {
        use crate::trigger::collection_workflow::CollectionDeliveryRole;

        assert_eq!(
            collection_control_skip_reason(
                Some(CollectionDeliveryRole::MemberDescendant),
                "stale collection control epoch at target commit",
            ),
            Some("CollectionControlBeforeDescendantCommit")
        );
        assert_eq!(
            collection_control_skip_reason(
                Some(CollectionDeliveryRole::Member),
                "collection member descendant was fenced by lifecycle change",
            ),
            None
        );
    }
}
