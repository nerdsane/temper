//! Production (async) dispatcher for cross-entity reactions.
//!
//! [`ReactionDispatcher`] evaluates reaction rules after a successful entity
//! action and asynchronously dispatches target actions via [`ServerState`].
//! Fire-and-forget: the source transition is already committed regardless of
//! reaction outcome.

use std::sync::Arc;

use crate::request_context::AgentContext;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;
use tracing;
use tracing::instrument;

use super::registry::ReactionRegistry;
use super::types::{MAX_REACTION_DEPTH, ReactionResult};

#[derive(Clone)]
struct BoundDelivery {
    delivery_id: String,
    root_delivery_id: String,
    fencing_token: u64,
    target_entity_id: Option<String>,
}

/// Async reaction dispatcher for production use.
///
/// Holds a shared [`ReactionRegistry`] and dispatches target actions through
/// the server state. Cascade is bounded by [`MAX_REACTION_DEPTH`].
pub struct ReactionDispatcher {
    registry: Arc<ReactionRegistry>,
}

impl ReactionDispatcher {
    /// Create a new dispatcher with the given registry.
    pub fn new(registry: Arc<ReactionRegistry>) -> Self {
        Self { registry }
    }

    /// Snapshot every rule that may fire for a source action before it commits.
    pub(crate) fn candidate_rules(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
    ) -> Vec<super::types::ReactionRule> {
        self.registry
            .candidates(tenant, entity_type, action)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Tenants whose registries may own durable delivery intents.
    pub(crate) fn tenant_ids(&self) -> Vec<TenantId> {
        self.registry.tenant_ids()
    }

    /// Scan committed source events and deliver non-terminal intents within a
    /// caller-supplied work budget. This is the restart recovery entry point.
    pub async fn recover_tenant_deliveries(
        &self,
        state: &crate::ServerState,
        tenant: &TenantId,
        work_budget: usize,
    ) -> Result<usize, String> {
        use crate::trigger::delivery::{
            REACTION_DELIVERY_ENTITY_TYPE, ReactionDeliveryStatus, extract_intents,
            load_delivery_record,
        };

        if work_budget == 0 {
            return Ok(0);
        }
        let (store, _) = state
            .event_journal()
            .ok_or_else(|| "durable reaction recovery requires an event journal".to_string())?;
        let entities = store
            .list_entity_ids_limited(tenant.as_str(), None, 10_000)
            .await
            .map_err(|error| error.to_string())?;
        let mut recovered = 0usize;
        for (entity_type, entity_id) in entities {
            if entity_type == REACTION_DELIVERY_ENTITY_TYPE || recovered >= work_budget {
                continue;
            }
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let events = store
                .read_events(&persistence_id, 0)
                .await
                .map_err(|error| error.to_string())?;
            for event in events {
                for intent in extract_intents(&event.payload).map_err(|error| error.to_string())? {
                    if recovered >= work_budget {
                        return Ok(recovered);
                    }
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
                    if record
                        .next_attempt_at
                        .is_some_and(|next| next > temper_runtime::scheduler::sim_now())
                    {
                        continue;
                    }
                    match self.dispatch_committed_intent(state, intent).await {
                        Ok(_) => recovered = recovered.saturating_add(1),
                        Err(error) if error == "reaction delivery is already leased" => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
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
            total = total.saturating_add(
                self.recover_tenant_deliveries(state, tenant, work_budget)
                    .await?,
            );
            let Some(delay) = self.next_tenant_delivery_delay(state, tenant).await? else {
                return Ok(total);
            };
            let now = tokio::time::Instant::now(); // determinism-ok: caller wall-time budget, not persisted ordering
            if now >= deadline {
                return Ok(total);
            }
            tokio::time::sleep(delay.min(deadline - now)).await; // determinism-ok: production delivery worker; persisted scheduler timestamps determine eligibility
        }
    }

    async fn next_tenant_delivery_delay(
        &self,
        state: &crate::ServerState,
        tenant: &TenantId,
    ) -> Result<Option<std::time::Duration>, String> {
        use crate::trigger::delivery::{ReactionDeliveryStatus, list_delivery_records};

        let (store, _) = state
            .event_journal()
            .ok_or_else(|| "durable reaction recovery requires an event journal".to_string())?;
        let records = list_delivery_records(&store, tenant.as_str(), 10_000)
            .await
            .map_err(|error| error.to_string())?;
        let now = temper_runtime::scheduler::sim_now();
        Ok(records
            .into_iter()
            .filter_map(|(record, _)| match record.status {
                ReactionDeliveryStatus::Pending => record
                    .next_attempt_at
                    .map(|next| next.signed_duration_since(now).to_std().unwrap_or_default())
                    .or(Some(std::time::Duration::ZERO)),
                ReactionDeliveryStatus::Claimed | ReactionDeliveryStatus::Dispatching => record
                    .lease_expires_at
                    .map(|expiry| {
                        expiry
                            .signed_duration_since(now)
                            .to_std()
                            .unwrap_or_default()
                    })
                    .or(Some(std::time::Duration::ZERO)),
                ReactionDeliveryStatus::Succeeded
                | ReactionDeliveryStatus::Skipped
                | ReactionDeliveryStatus::DroppedAllowed
                | ReactionDeliveryStatus::Rejected
                | ReactionDeliveryStatus::DeadLettered => None,
            })
            .min())
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

        let rule: super::types::ReactionRule = match serde_json::from_value(intent.rule.clone()) {
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
            let target_persistence_id = format!(
                "{}:{}:{}",
                intent.tenant, rule.then.entity_type, target_entity_id
            );
            let target_events = store
                .read_events(&target_persistence_id, 0)
                .await
                .map_err(|error| error.to_string())?;
            let matching_receipt = target_events.iter().any(|event| {
                crate::trigger::delivery::extract_receipt(&event.payload)
                    .ok()
                    .flatten()
                    .is_some_and(|receipt| receipt.delivery_id == intent.delivery_id)
            });
            if matching_receipt {
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

    /// Dispatch reactions triggered by a successful entity action.
    ///
    /// This is called after the source action has been committed and the SSE
    /// broadcast sent. Reactions are fire-and-forget: failures are logged but
    /// do not roll back the source transition.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(
        otel.name = "reaction.dispatch",
        tenant = %tenant,
        entity_type,
        entity_id,
        action_name = action,
        depth,
        reaction.rule_count = tracing::field::Empty,
        reaction.fired_count = tracing::field::Empty,
        reaction.guard_skipped_count = tracing::field::Empty,
        reaction.target_resolve_error_count = tracing::field::Empty,
        reaction.authz_denied_count = tracing::field::Empty,
        reaction.dispatch_error_count = tracing::field::Empty,
        reaction.success_count = tracing::field::Empty,
        reaction.result_count = tracing::field::Empty,
    ))]
    pub async fn dispatch_reactions(
        &self,
        state: &crate::ServerState,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        to_state: &str,
        fields: &serde_json::Value,
        depth: u32,
        invoking_ctx: &AgentContext,
    ) -> Vec<ReactionResult> {
        let rules: Vec<_> = self
            .registry
            .lookup(tenant, entity_type, action, to_state)
            .into_iter()
            .cloned()
            .collect();

        self.dispatch_rules(
            state,
            tenant,
            entity_type,
            entity_id,
            action,
            to_state,
            fields,
            depth,
            invoking_ctx,
            rules,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch_rules(
        &self,
        state: &crate::ServerState,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        to_state: &str,
        fields: &serde_json::Value,
        depth: u32,
        invoking_ctx: &AgentContext,
        rules: Vec<super::types::ReactionRule>,
        bound_delivery: Option<BoundDelivery>,
    ) -> Vec<ReactionResult> {
        if depth >= MAX_REACTION_DEPTH {
            record_reaction_fanout_span(ReactionFanoutCounts::default());
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                action,
                depth,
                "Reaction cascade depth limit reached ({MAX_REACTION_DEPTH})"
            );
            return Vec::new();
        }

        if rules.is_empty() {
            record_reaction_fanout_span(ReactionFanoutCounts::default());
            return Vec::new();
        }

        let rule_count = rules.len();
        let mut fired_count = 0usize;
        let mut guard_skipped_count = 0usize;
        let mut target_resolve_error_count = 0usize;
        let mut authz_denied_count = 0usize;
        let mut dispatch_error_count = 0usize;
        let mut success_count = 0usize;
        let mut results = Vec::new();

        for rule in rules {
            // Guard evaluation: skip rules whose guard evaluates to false.
            // Guard-skipped rules do not produce a `ReactionResult` — they
            // never fired.
            if let Some(guard) = &rule.when.guard {
                let mut queries = Vec::new();
                super::guard::collect_cross_entity_queries(guard, fields, &mut queries);
                let mut resolved = super::guard::CrossStatusMap::new();
                for q in &queries {
                    let status = state
                        .resolve_entity_status(tenant, &q.entity_type, &q.target_entity_id)
                        .await;
                    let matched = status.as_deref().map(|s| q.matches(s)).unwrap_or(false);
                    resolved.insert(q.key(), matched);
                }
                let passed = super::guard::evaluate_with_resolved(
                    guard, fields, to_state, &resolved, &rule.name,
                );
                if !passed {
                    guard_skipped_count += 1;
                    tracing::debug!(
                        rule = rule.name,
                        cross_entity_queries = queries.len(),
                        "reaction guard failed; skipping rule"
                    );
                    continue;
                }
            }

            let target_entity_id = match bound_delivery
                .as_ref()
                .and_then(|delivery| delivery.target_entity_id.clone())
                .or_else(|| {
                    super::resolver::resolve_target_id(&rule.resolve_target, entity_id, fields)
                }) {
                Some(id) => id,
                None => {
                    target_resolve_error_count += 1;
                    tracing::warn!(
                        rule = rule.name,
                        "Could not resolve target entity ID for reaction"
                    );
                    results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: false,
                        target_status: None,
                        error: Some("Could not resolve target entity ID".to_string()),
                        depth,
                    });
                    continue;
                }
            };

            tracing::info!(
                rule = rule.name,
                source_entity = %entity_type,
                source_id = %entity_id,
                target_entity = %rule.then.entity_type,
                target_id = %target_entity_id,
                target_action = %rule.then.action,
                depth,
                "Dispatching reaction"
            );

            let effective_params =
                super::params::build_effective_params(&rule.then, entity_id, fields, &rule.name);

            // ADR-0046: resolve the dispatch principal. If the rule declares
            // an explicit `principal`, build a synthetic service identity;
            // otherwise inherit the invoking principal's exact
            // `SecurityContext` when available.
            let mut dispatch_ctx = resolve_trigger_principal(
                rule.principal.as_deref(),
                invoking_ctx,
                &rule.name,
                entity_type,
                entity_id,
                action,
            );
            if let Some(delivery) = bound_delivery.as_ref() {
                dispatch_ctx.idempotency_key = Some(delivery.delivery_id.clone());
            }

            let authz_snapshot = match state
                .load_authz_resource_snapshot(tenant, &rule.then.entity_type, &target_entity_id)
                .await
            {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    tracing::warn!(
                        rule = rule.name,
                        target_entity = %rule.then.entity_type,
                        target_id = %target_entity_id,
                        error = %e,
                        "Reaction authz snapshot failed"
                    );
                    results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: false,
                        target_status: None,
                        error: Some(e),
                        depth,
                    });
                    continue;
                }
            };

            let security_ctx = effective_trigger_security_context(&dispatch_ctx);
            if let Err(denial) = state.authorize_with_context(
                &security_ctx,
                &rule.then.action,
                &rule.then.entity_type,
                &authz_snapshot.resource_attrs,
                tenant.as_str(),
            ) {
                let reason = denial.to_string();
                authz_denied_count += 1;
                tracing::warn!(
                    rule = rule.name,
                    target_entity = %rule.then.entity_type,
                    target_id = %target_entity_id,
                    target_action = %rule.then.action,
                    principal_id = %security_ctx.principal.id,
                    principal_kind = ?security_ctx.principal.kind,
                    "Reaction authorization denied: {reason}"
                );
                results.push(ReactionResult {
                    rule_name: rule.name.clone(),
                    success: false,
                    target_status: Some(authz_snapshot.current_state.state.status.clone()),
                    error: Some(reason),
                    depth,
                });
                continue;
            }

            // Fire the target action via the core dispatch (no reaction cascade
            // to avoid infinite async recursion — we handle cascading ourselves).
            fired_count += 1;
            let reaction_context = if let Some(delivery) = bound_delivery.as_ref() {
                let authority =
                    serde_json::to_value(effective_trigger_security_context(&dispatch_ctx))
                        .map_err(|error| error.to_string());
                match authority {
                    Ok(authority) => Some(crate::trigger::delivery::ReactionCommitContext {
                        rules: self.candidate_rules(
                            tenant,
                            &rule.then.entity_type,
                            &rule.then.action,
                        ),
                        authority,
                        depth: depth + 1,
                        root_delivery_id: Some(delivery.root_delivery_id.clone()),
                        receipt: Some(crate::trigger::delivery::ReactionReceipt {
                            delivery_id: delivery.delivery_id.clone(),
                            fencing_token: delivery.fencing_token,
                            received_at: temper_runtime::scheduler::sim_now(),
                        }),
                    }),
                    Err(error) => {
                        dispatch_error_count += 1;
                        results.push(ReactionResult {
                            rule_name: rule.name.clone(),
                            success: false,
                            target_status: None,
                            error: Some(error),
                            depth,
                        });
                        continue;
                    }
                }
            } else {
                None
            };
            let dispatch_result = state
                .dispatch_tenant_action_core(
                    tenant,
                    &rule.then.entity_type,
                    &target_entity_id,
                    &rule.then.action,
                    effective_params,
                    &dispatch_ctx,
                    false,
                    reaction_context,
                )
                .await;

            match dispatch_result {
                Ok(response) => {
                    let target_status = response.state.status.clone();
                    if response.success {
                        success_count += 1;
                    }
                    results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: response.success,
                        target_status: Some(target_status.clone()),
                        error: if response.success {
                            None
                        } else {
                            response.error.clone()
                        },
                        depth,
                    });

                    // Recurse if the target action succeeded. The cascade
                    // fires under the same dispatch context as this rule —
                    // elevation propagates down the chain.
                    if response.success && bound_delivery.is_none() {
                        let cascade_results = Box::pin(self.dispatch_reactions(
                            state,
                            tenant,
                            &rule.then.entity_type,
                            &target_entity_id,
                            &rule.then.action,
                            &target_status,
                            &serde_json::to_value(&response.state.fields).unwrap_or_default(),
                            depth + 1,
                            &dispatch_ctx,
                        ))
                        .await;
                        results.extend(cascade_results);
                    }
                }
                Err(e) => {
                    dispatch_error_count += 1;
                    tracing::warn!(
                        rule = rule.name,
                        error = %e,
                        "Reaction dispatch failed"
                    );
                    results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: false,
                        target_status: None,
                        error: Some(e.to_string()),
                        depth,
                    });
                }
            }
        }

        record_reaction_fanout_span(ReactionFanoutCounts {
            rule_count,
            fired_count,
            guard_skipped_count,
            target_resolve_error_count,
            authz_denied_count,
            dispatch_error_count,
            success_count,
            result_count: results.len(),
        });

        results
    }
}

#[derive(Default)]
struct ReactionFanoutCounts {
    rule_count: usize,
    fired_count: usize,
    guard_skipped_count: usize,
    target_resolve_error_count: usize,
    authz_denied_count: usize,
    dispatch_error_count: usize,
    success_count: usize,
    result_count: usize,
}

fn record_reaction_fanout_span(counts: ReactionFanoutCounts) {
    let span = tracing::Span::current();
    span.record("reaction.rule_count", counts.rule_count);
    span.record("reaction.fired_count", counts.fired_count);
    span.record("reaction.guard_skipped_count", counts.guard_skipped_count);
    span.record(
        "reaction.target_resolve_error_count",
        counts.target_resolve_error_count,
    );
    span.record("reaction.authz_denied_count", counts.authz_denied_count);
    span.record("reaction.dispatch_error_count", counts.dispatch_error_count);
    span.record("reaction.success_count", counts.success_count);
    span.record("reaction.result_count", counts.result_count);
}

fn is_transient_delivery_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    [
        "timeout",
        "temporar",
        "mailbox",
        "deferred",
        "connection",
        "storage",
        "unavailable",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_expected_target_drop(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("not valid from state") || normalized.contains("blocked from state")
}

fn automatic_retry_backoff(attempts: u32) -> chrono::Duration {
    match attempts {
        0 | 1 => chrono::Duration::milliseconds(100),
        2 => chrono::Duration::milliseconds(500),
        3 => chrono::Duration::seconds(2),
        _ => chrono::Duration::seconds(5),
    }
}

fn record_delivery_terminal_metrics(record: &crate::trigger::delivery::ReactionDeliveryRecord) {
    use crate::trigger::delivery::ReactionDeliveryStatus;

    let outcome = match record.status {
        ReactionDeliveryStatus::Succeeded => "succeeded",
        ReactionDeliveryStatus::Skipped => "skipped",
        ReactionDeliveryStatus::DroppedAllowed => "dropped_allowed",
        ReactionDeliveryStatus::Rejected => "rejected",
        ReactionDeliveryStatus::DeadLettered => "dead_lettered",
        ReactionDeliveryStatus::Pending
        | ReactionDeliveryStatus::Claimed
        | ReactionDeliveryStatus::Dispatching => return,
    };
    let age = temper_runtime::scheduler::sim_now()
        .signed_duration_since(record.intent.created_at)
        .to_std()
        .unwrap_or_default();
    crate::runtime_metrics::record_reaction_delivery_outcome(outcome, record.attempts, age);
}

// Target resolver logic consolidated in super::resolver::resolve_target_id.

/// ADR-0046: resolve the dispatch context for a reaction.
///
/// - When the rule declares a principal (`Some(service_name)`), build a
///   synthetic `AgentContext` identifying the named service. Cedar policies
///   can match on `principal.role`, `principal.agent_type`, or
///   `principal.id == Service::"<name>"` — whichever style the tenant
///   prefers. The `agent_type` slot carries the name so the Cedar request
///   sees it via `principal.agent_type == "<name>"`.
/// - When the rule has no principal (`None`), inherit the invoking context
///   directly — the trigger runs as whoever called the source action.
fn resolve_trigger_principal(
    declared_principal: Option<&str>,
    invoking_ctx: &AgentContext,
    rule_name: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    source_action: &str,
) -> AgentContext {
    match declared_principal {
        Some(service_name) if !service_name.is_empty() => {
            let mut ctx = AgentContext::for_service_inheriting(service_name, invoking_ctx);
            // Preserve ADR-0048 behavior for declared reaction principals:
            // existing trigger dispatch copied the caller's idempotency key.
            ctx.idempotency_key = invoking_ctx.idempotency_key.clone();
            if let Some(security_ctx) = ctx.security_ctx.as_mut() {
                security_ctx.context_attrs.insert(
                    "triggerRule".to_string(),
                    serde_json::Value::String(rule_name.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerSourceEntityType".to_string(),
                    serde_json::Value::String(source_entity_type.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerSourceEntityId".to_string(),
                    serde_json::Value::String(source_entity_id.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerSourceAction".to_string(),
                    serde_json::Value::String(source_action.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerDeclaredPrincipal".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            ctx
        }
        _ => invoking_ctx.clone(),
    }
}

pub(crate) fn effective_trigger_security_context(agent_ctx: &AgentContext) -> SecurityContext {
    if let Some(security_ctx) = &agent_ctx.security_ctx {
        return security_ctx.clone();
    }

    let mut security_ctx = SecurityContext::from_headers(&[]).with_agent_context(
        agent_ctx.agent_id.as_deref(),
        agent_ctx.session_id.as_deref(),
        agent_ctx.agent_type.as_deref(),
    );
    security_ctx.context_attrs.insert(
        "triggerInheritedContextApproximate".to_string(),
        serde_json::Value::Bool(true),
    );
    security_ctx
}

#[cfg(test)]
mod tests {
    use super::is_expected_target_drop;

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
