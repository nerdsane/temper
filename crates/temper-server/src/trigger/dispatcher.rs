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

        let rules: Vec<_> = self
            .registry
            .lookup(tenant, entity_type, action, to_state)
            .into_iter()
            .cloned()
            .collect();

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

            let target_entity_id =
                match super::resolver::resolve_target_id(&rule.resolve_target, entity_id, fields) {
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
            let dispatch_ctx = resolve_trigger_principal(
                rule.principal.as_deref(),
                invoking_ctx,
                &rule.name,
                entity_type,
                entity_id,
                action,
            );

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
            let dispatch_result = state
                .dispatch_tenant_action_core(
                    tenant,
                    &rule.then.entity_type,
                    &target_entity_id,
                    &rule.then.action,
                    effective_params,
                    &dispatch_ctx,
                    false,
                    None,
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
                    if response.success {
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

fn effective_trigger_security_context(agent_ctx: &AgentContext) -> SecurityContext {
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
