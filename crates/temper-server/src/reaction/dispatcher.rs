//! Production (async) dispatcher for cross-entity reactions.
//!
//! [`ReactionDispatcher`] evaluates reaction rules after a successful entity
//! action and asynchronously dispatches target actions via [`ServerState`].
//! Fire-and-forget: the source transition is already committed regardless of
//! reaction outcome.

use std::sync::Arc;

use crate::request_context::AgentContext;
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
    #[instrument(skip_all, fields(otel.name = "reaction.dispatch", tenant = %tenant, entity_type, entity_id, action_name = action, depth))]
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
            return Vec::new();
        }

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
                super::params::build_effective_params(&rule.then, fields, &rule.name);

            // ADR-0046: resolve the dispatch principal. If the rule declares
            // an explicit `principal`, build a synthetic elevated context;
            // otherwise inherit the invoking principal. Reactions loaded
            // from legacy `reactions.toml` have `principal = None` and thus
            // inherit — preserving the pre-ADR-0046 semantics for them.
            let dispatch_ctx = resolve_trigger_principal(rule.principal.as_deref(), invoking_ctx);

            // Fire the target action via the core dispatch (no reaction cascade
            // to avoid infinite async recursion — we handle cascading ourselves).
            let dispatch_result = state
                .dispatch_tenant_action_core(
                    tenant,
                    &rule.then.entity_type,
                    &target_entity_id,
                    &rule.then.action,
                    effective_params,
                    &dispatch_ctx,
                    false,
                )
                .await;

            match dispatch_result {
                Ok(response) => {
                    let target_status = response.state.status.clone();
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

        results
    }
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
///   directly — the reaction runs as whoever called the source action.
///   For legacy reactions loaded from `reactions.toml` (which had no
///   principal concept), this preserves pre-ADR-0046 behavior when the
///   invoker was already `AgentContext::system()`.
fn resolve_trigger_principal(
    declared_principal: Option<&str>,
    invoking_ctx: &AgentContext,
) -> AgentContext {
    match declared_principal {
        Some(service_name) if !service_name.is_empty() => {
            // Synthetic elevated context. Clone other fields (trace_id,
            // session_id, idempotency_key) from the invoker so observability
            // continuity is preserved across the reaction hop.
            let mut ctx = invoking_ctx.clone();
            ctx.agent_id = Some(format!("service:{service_name}"));
            ctx.agent_type = Some(service_name.to_string());
            ctx
        }
        _ => invoking_ctx.clone(),
    }
}
