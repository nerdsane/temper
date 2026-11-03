//! Production (async) dispatcher for cross-entity reactions.
//!
//! [`ReactionDispatcher`] evaluates reaction rules after a successful entity
//! action and asynchronously dispatches target actions via [`ServerState`].
//! Fire-and-forget: the source transition is already committed regardless of
//! reaction outcome.

use std::sync::Arc;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;

use crate::odata::read_defaults::ReadDefaults;
use crate::request_context::AgentContext;
use temper_runtime::tenant::TenantId;
use tracing;
use tracing::instrument;

use super::owed_reactions::{Reaction, ReactionTry};
use super::principal::{effective_trigger_security_context, resolve_trigger_principal};
use super::registry::ReactionRegistry;
use super::repeat_stop::RepeatStop;
use super::types::ReactionResult;
use crate::state::owed_writes::OwedMove;

/// Async reaction dispatcher for production use.
///
/// Holds a shared [`ReactionRegistry`] and dispatches target actions through
/// the server state. A chain stops the first time a reaction would repeat a
/// step it is already inside of ([`InlineChain`](crate::request_context::InlineChain)
/// says why); the reaction it stops is reported, not dropped.
pub struct ReactionDispatcher {
    registry: Arc<ReactionRegistry>,
}

/// The action whose reactions one level of a chain runs, `depth` levels in.
struct Level<'a> {
    entity_type: &'a str,
    entity_id: &'a str,
    action: &'a str,
    to_state: &'a str,
    fields: &'a serde_json::Value,
    depth: u32,
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
    ///
    /// A chain is as deep as its data, so every level, the first included, is
    /// boxed and polled on a stack that grows: a caller's future holds a
    /// pointer, not the level, and a deep chain gets more stack instead of
    /// overflowing its thread. The named return type is what lets the
    /// recursive future be `Send`.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_reactions<'a>(
        &'a self,
        state: &'a crate::ServerState,
        tenant: &'a TenantId,
        entity_type: &'a str,
        entity_id: &'a str,
        action: &'a str,
        to_state: &'a str,
        fields: &'a serde_json::Value,
        depth: u32,
        invoking_ctx: &'a AgentContext,
    ) -> BoxFuture<'a, Vec<ReactionResult>> {
        crate::growable_stack::on_growable_stack(move || {
            let level = Level {
                entity_type,
                entity_id,
                action,
                to_state,
                fields,
                depth,
            };
            self.dispatch_level(state, tenant, level, invoking_ctx)
                .boxed()
        })
    }

    /// One level of a chain: the reactions of one action.
    #[instrument(skip_all, fields(
        otel.name = "reaction.dispatch",
        tenant = %tenant,
        entity_type = level.entity_type,
        entity_id = level.entity_id,
        action_name = level.action,
        depth = level.depth,
        reaction.rule_count = tracing::field::Empty,
        reaction.fired_count = tracing::field::Empty,
        reaction.guard_skipped_count = tracing::field::Empty,
        reaction.target_resolve_error_count = tracing::field::Empty,
        reaction.authz_denied_count = tracing::field::Empty,
        reaction.dispatch_error_count = tracing::field::Empty,
        reaction.repeat_stopped_count = tracing::field::Empty,
        reaction.success_count = tracing::field::Empty,
        reaction.result_count = tracing::field::Empty,
    ))]
    async fn dispatch_level(
        &self,
        state: &crate::ServerState,
        tenant: &TenantId,
        level: Level<'_>,
        invoking_ctx: &AgentContext,
    ) -> Vec<ReactionResult> {
        let Level {
            entity_type,
            entity_id,
            action,
            to_state,
            fields,
            depth,
        } = level;
        // The source action is the step these reactions continue from.
        let source_ctx = invoking_ctx.entering(entity_type, entity_id, action);
        let invoking_ctx = &source_ctx;
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
        let mut repeat_stopped_count = 0usize;
        let mut success_count = 0usize;
        let mut results = Vec::new();
        // Guards and args read the source record as every guard does: a
        // variable it stores no value for reads as its declared initial value.
        let reads_record = rules
            .iter()
            .any(|rule| rule.when.guard.is_some() || !rule.then.args.is_empty());
        let read = reads_record
            .then(|| ReadDefaults::for_entity(state, tenant, entity_type).fields_as_read(fields));

        for rule in rules {
            // Guard evaluation: skip rules whose guard evaluates to false.
            // Guard-skipped rules do not produce a `ReactionResult` — they
            // never fired.
            if let (Some(guard), Some(read)) = (&rule.when.guard, &read) {
                let mut related = temper_jit::table::RelatedMap::new();
                for (reference, ids) in super::guard::related_ids(guard, read) {
                    let mut statuses = Vec::with_capacity(ids.len());
                    for id in &ids {
                        statuses.push(state.resolve_entity_status(tenant, &reference.0, id).await);
                    }
                    related.insert(reference, temper_jit::table::Related::Statuses(statuses));
                }
                if !super::guard::guard_holds(guard, read, to_state, &related) {
                    guard_skipped_count += 1;
                    tracing::debug!(
                        rule = rule.name,
                        related_references = related.len(),
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

            // A target the rule makes is new, and the chain knows it as made.
            let mut chain = invoking_ctx.inline_chain.clone();
            if super::resolver::mints_target_id(&rule.resolve_target, fields) {
                chain = chain.making(&rule.then.entity_type, &target_entity_id);
            }
            let step = chain.step(&rule.then.entity_type, &target_entity_id, &rule.then.action);
            let chain = match chain.then(step) {
                Ok(chain) => chain,
                Err(repeat) => {
                    repeat_stopped_count += 1;
                    RepeatStop {
                        tenant,
                        entity_type,
                        entity_id,
                        action,
                        rule: &rule,
                        target_entity_id: &target_entity_id,
                        repeat: &repeat,
                        depth,
                    }
                    .record(state);
                    results.push(ReactionResult::stopped_on_repeat(
                        &rule.name, depth, &repeat,
                    ));
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

            let effective_params = super::params::build_effective_params(
                &rule.then,
                entity_id,
                read.as_ref().unwrap_or(fields),
                &rule.name,
            );

            // ADR-0046: resolve the dispatch principal. If the rule declares
            // an explicit `principal`, build a synthetic service identity;
            // otherwise inherit the invoking principal's exact
            // `SecurityContext` when available. Either way it carries how this
            // move comes about: this rule, set off by the invoking move. Cedar
            // sees it as `context.via` and the rest, so an app can tell a move
            // the spec makes follow another from a principal's own pick. The
            // principal itself stays the true one.
            let (mut dispatch_ctx, cause) = resolve_trigger_principal(
                rule.principal.as_deref(),
                invoking_ctx,
                &rule.name,
                entity_type,
                entity_id,
                action,
                depth,
            );
            dispatch_ctx.inline_chain = chain;

            // Read the target, authorize the move on it as it is now, and fire
            // it via the core dispatch (no reaction cascade to avoid infinite
            // async recursion — we handle cascading ourselves). A try whose
            // read or send the store failed is kept and tried again, read and
            // authorization included (`owed_reactions`).
            let security_ctx = effective_trigger_security_context(&dispatch_ctx, &cause);
            let reaction = Reaction {
                sent: OwedMove {
                    tenant: tenant.clone(),
                    entity_type: rule.then.entity_type.clone(),
                    entity_id: target_entity_id.clone(),
                    action: rule.then.action.clone(),
                    params: effective_params,
                    agent_ctx: dispatch_ctx.clone(),
                },
                authorized_as: security_ctx.clone(),
            };
            let tried = state.dispatch_reaction(reaction, depth).await;
            let dispatch_result = match tried {
                ReactionTry::Unread(e) => {
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
                        error: Some(e.to_string()),
                        depth,
                    });
                    continue;
                }
                ReactionTry::Denied {
                    reason,
                    target_status,
                } => {
                    authz_denied_count += 1;
                    results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: false,
                        target_status: Some(target_status),
                        error: Some(reason),
                        depth,
                    });
                    continue;
                }
                ReactionTry::Sent(reply) => *reply,
            };
            fired_count += 1;

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
                    // elevation propagates down the chain — and knows the
                    // children the action spawned as entities it made. A
                    // chain is as deep as its data, so each level is polled
                    // on a stack that grows.
                    if response.success {
                        let cascade_ctx =
                            dispatch_ctx.having_made(response.spawn_requests.iter().map(|spawn| {
                                (spawn.entity_type.as_str(), spawn.entity_id.as_str())
                            }));
                        let target_fields =
                            serde_json::to_value(&response.state.fields).unwrap_or_default();
                        let cascade_results = self
                            .dispatch_reactions(
                                state,
                                tenant,
                                &rule.then.entity_type,
                                &target_entity_id,
                                response.action(&rule.then.action),
                                &target_status,
                                &target_fields,
                                depth + 1,
                                &cascade_ctx,
                            )
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
            repeat_stopped_count,
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
    repeat_stopped_count: usize,
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
    span.record("reaction.repeat_stopped_count", counts.repeat_stopped_count);
    span.record("reaction.success_count", counts.success_count);
    span.record("reaction.result_count", counts.result_count);
}

#[cfg(test)]
#[path = "dispatcher_test.rs"]
mod tests;
