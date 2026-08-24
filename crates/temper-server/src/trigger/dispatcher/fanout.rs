//! Reaction rule evaluation, authorization, dispatch, and telemetry.

mod entry;

use crate::request_context::AgentContext;
use crate::trigger::{guard, params, resolver};
use temper_runtime::tenant::TenantId;

use super::super::types::{MAX_REACTION_DEPTH, ReactionResult, ReactionRule};
use super::{BoundDelivery, ReactionDispatcher, effective_trigger_security_context};

impl ReactionDispatcher {
    #[expect(
        clippy::too_many_arguments,
        reason = "reaction dispatch binds source authority and transition identity"
    )]
    pub(super) async fn dispatch_rules(
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
        rules: Vec<ReactionRule>,
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
            // Legacy non-durable fanout evaluates guards at dispatch time.
            // Guard-skipped rules do not produce a `ReactionResult` — they
            // never fired.
            if bound_delivery.is_none()
                && let Some(guard) = &rule.when.guard
            {
                let mut queries = Vec::new();
                guard::collect_cross_entity_queries(guard, fields, &mut queries);
                let mut resolved = guard::CrossStatusMap::new();
                for q in &queries {
                    let status = match invoking_ctx.schema_pin.as_ref() {
                        Some(pin) => state
                            .get_scoped_entity_state(
                                tenant,
                                &q.entity_type,
                                &q.target_entity_id,
                                pin.clone(),
                            )
                            .await
                            .ok()
                            .map(|response| response.state.status),
                        None => {
                            state
                                .resolve_entity_status(tenant, &q.entity_type, &q.target_entity_id)
                                .await
                        }
                    };
                    let matched = status.as_deref().map(|s| q.matches(s)).unwrap_or(false);
                    resolved.insert(q.key(), matched);
                }
                let passed =
                    guard::evaluate_with_resolved(guard, fields, to_state, &resolved, &rule.name);
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
                .or_else(|| resolver::resolve_target_id(&rule.resolve_target, entity_id, fields))
            {
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
                params::build_effective_params(&rule.then, entity_id, fields, &rule.name);

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
                dispatch_ctx.expected_entity_sequence = delivery.expected_target_sequence;
            }

            let authz_snapshot = match dispatch_ctx.schema_pin.as_ref() {
                Some(pin) => state
                    .get_or_initialize_scoped_entity_state(
                        tenant,
                        &rule.then.entity_type,
                        &target_entity_id,
                        pin.clone(),
                    )
                    .await
                    .map(|response| {
                        let mut attrs = response
                            .state
                            .fields
                            .as_object()
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .collect::<std::collections::BTreeMap<_, _>>();
                        attrs.insert(
                            "id".into(),
                            serde_json::Value::String(target_entity_id.clone()),
                        );
                        attrs.insert(
                            "status".into(),
                            serde_json::Value::String(response.state.status.clone()),
                        );
                        attrs.insert("has_spec".into(), serde_json::Value::Bool(true));
                        (attrs, response.state.status)
                    }),
                None => state
                    .load_authz_resource_snapshot(tenant, &rule.then.entity_type, &target_entity_id)
                    .await
                    .map(|snapshot| (snapshot.resource_attrs, snapshot.current_state.state.status)),
            };
            let (authz_resource_attrs, authz_status) = match authz_snapshot {
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
                &authz_resource_attrs,
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
                    target_status: Some(authz_status),
                    error: Some(reason),
                    depth,
                });
                continue;
            }

            // Fire the target action via the core dispatch (no reaction cascade
            // to avoid infinite async recursion — we handle cascading ourselves).
            fired_count += 1;
            let reaction_context = if let Some(delivery) = bound_delivery.as_ref() {
                let descendant_rules = if let Some(pin) = dispatch_ctx.schema_pin.as_ref() {
                    state
                        .registry
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .scoped_reaction_candidates_at_digest(
                            tenant,
                            &pin.scope,
                            &pin.bundle_digest,
                            &rule.then.entity_type,
                            &rule.then.action,
                        )
                } else {
                    self.candidate_rules(tenant, &rule.then.entity_type, &rule.then.action)
                };
                let guard_source = match dispatch_ctx.schema_pin.as_ref() {
                    Some(pin) => state
                        .get_or_initialize_scoped_entity_state(
                            tenant,
                            &rule.then.entity_type,
                            &target_entity_id,
                            pin.clone(),
                        )
                        .await
                        .ok(),
                    None => state
                        .get_tenant_entity_state(tenant, &rule.then.entity_type, &target_entity_id)
                        .await
                        .ok(),
                };
                let expected_source_sequence = guard_source
                    .as_ref()
                    .map_or(0, |response| response.state.sequence_nr);
                let mut guard_fields = guard_source
                    .map(|response| response.state.fields)
                    .unwrap_or_else(|| serde_json::json!({}));
                if let (Some(fields), Some(params)) =
                    (guard_fields.as_object_mut(), effective_params.as_object())
                {
                    for (name, value) in params {
                        fields.insert(name.clone(), value.clone());
                    }
                }
                let resolved_guards = super::resolve_rule_guard_inputs(
                    state,
                    tenant,
                    &descendant_rules,
                    &guard_fields,
                    dispatch_ctx.schema_pin.as_ref(),
                )
                .await;
                let authority =
                    serde_json::to_value(effective_trigger_security_context(&dispatch_ctx))
                        .map_err(|error| error.to_string());
                match authority {
                    Ok(authority) => Some(crate::trigger::delivery::ReactionCommitContext {
                        rules: descendant_rules,
                        authority,
                        depth: depth + 1,
                        root_delivery_id: Some(delivery.root_delivery_id.clone()),
                        expected_source_sequence,
                        resolved_guards,
                        receipt: Some(crate::trigger::delivery::ReactionReceipt {
                            delivery_id: delivery.delivery_id.clone(),
                            fencing_token: delivery.fencing_token,
                            received_at: temper_runtime::scheduler::sim_now(),
                            state_timeout_state: delivery.state_timeout_state.clone(),
                            schema_pin: None,
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
                    None,
                )
                .await;

            match dispatch_result {
                Ok(response) => {
                    let target_status = response.state.status.clone();
                    let descendant_error = if response.success && bound_delivery.is_some() {
                        match state
                            .materialize_committed_reaction_intents(
                                tenant,
                                &rule.then.entity_type,
                                &target_entity_id,
                                response.state.sequence_nr,
                                dispatch_ctx.schema_pin.as_ref(),
                            )
                            .await
                        {
                            Ok(intents) => {
                                if !intents.is_empty() {
                                    self.notify_recovery(tenant);
                                }
                                None
                            }
                            Err(error) => Some(error.to_string()),
                        }
                    } else {
                        None
                    };
                    if response.success && descendant_error.is_none() {
                        success_count += 1;
                    }
                    results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: response.success && descendant_error.is_none(),
                        target_status: Some(target_status.clone()),
                        error: descendant_error.or_else(|| response.error.clone()),
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

/// Resolve an explicit reaction service principal or inherit the caller.
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
