//! Deterministic simulation dispatcher for cross-entity reactions.
//!
//! [`SimReactionSystem`] wraps a [`SimActorSystem`] and a [`ReactionRegistry`],
//! executing reaction cascades synchronously after each action. Fully
//! deterministic — no async, no tokio, no wall clock.

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{SimActorHandler, SimActorSystem, SimActorSystemConfig};
use temper_runtime::tenant::TenantId;

use super::registry::ReactionRegistry;
use super::types::ReactionResult;
use crate::odata::read_defaults::ReadDefaults;
use crate::request_context::{ChainStep, RepeatedStep, StepEntity};

/// Maps actor IDs to their (entity_type, entity_id) for target resolution.
struct ActorMeta {
    entity_type: String,
    entity_id: String,
}

/// Deterministic simulation system with reaction cascade support.
///
/// Wraps a real [`SimActorSystem`] and dispatches reaction rules synchronously
/// after each successful `step()`. A chain stops the first time a reaction
/// would repeat a step it is already in, as in production
/// ([`InlineChain`](crate::request_context::InlineChain)).
pub struct SimReactionSystem {
    inner: SimActorSystem,
    registry: ReactionRegistry,
    tenant: TenantId,
    /// Maps actor_id -> (entity_type, entity_id) for target resolution.
    actor_meta: BTreeMap<String, ActorMeta>,
    /// Maps "entity_type:entity_id" -> actor_id for reverse lookup.
    entity_to_actor: BTreeMap<String, String>,
    /// Collected reaction results from the last cascade.
    last_results: Vec<ReactionResult>,
    /// Each entity type's declarations, for types registered from a table.
    declared: BTreeMap<String, ReadDefaults>,
}

impl SimReactionSystem {
    /// Create a new simulation reaction system for a tenant.
    pub fn new(
        config: SimActorSystemConfig,
        registry: ReactionRegistry,
        tenant: impl Into<TenantId>,
    ) -> Self {
        Self {
            inner: SimActorSystem::new(config),
            registry,
            tenant: tenant.into(),
            actor_meta: BTreeMap::new(),
            entity_to_actor: BTreeMap::new(),
            last_results: Vec::new(),
            declared: BTreeMap::new(),
        }
    }

    /// Register an entity actor with its type and ID.
    pub fn register_actor(
        &mut self,
        actor_id: &str,
        entity_type: &str,
        entity_id: &str,
        handler: Box<dyn SimActorHandler>,
    ) {
        self.inner.register_actor(actor_id, handler);
        self.actor_meta.insert(
            actor_id.to_string(),
            ActorMeta {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
            },
        );
        let entity_key = format!("{entity_type}:{entity_id}");
        self.entity_to_actor
            .insert(entity_key, actor_id.to_string());
    }

    /// Register an entity actor, building the handler from an IOA TOML spec.
    ///
    /// Convenience method that creates an [`EntityActorHandler`] from a
    /// [`TransitionTable`].
    pub fn register_entity(
        &mut self,
        actor_id: &str,
        entity_type: &str,
        entity_id: &str,
        table: Arc<TransitionTable>,
    ) {
        use crate::entity_actor::EntityActorHandler;
        self.declared
            .insert(entity_type.to_string(), ReadDefaults::from_table(&table));
        let handler = EntityActorHandler::new(entity_type, entity_id, table);
        self.register_actor(actor_id, entity_type, entity_id, Box::new(handler));
    }

    /// Execute an action and synchronously dispatch any matching reactions.
    ///
    /// Returns the result of the primary action. Reaction results are collected
    /// in [`last_results()`].
    pub fn step(
        &mut self,
        actor_id: &str,
        action: &str,
        params: &str,
    ) -> Result<serde_json::Value, String> {
        self.last_results.clear();

        // Execute the primary action
        let result = self.inner.step(actor_id, action, params)?;

        // Get the actor's entity info for reaction lookup
        let meta = self
            .actor_meta
            .get(actor_id)
            .ok_or_else(|| format!("No metadata for actor '{actor_id}'"))?;
        let entity_type = meta.entity_type.clone();
        let entity_id = meta.entity_id.clone();
        let to_state = self.inner.status(actor_id);

        // Build fields from the result for target resolution
        let fields = result.get("fields").unwrap_or(&result).clone();

        // Dispatch reactions recursively, starting from the action just run.
        let mut chain = vec![found_step(&entity_type, &entity_id, action)];
        self.dispatch_cascade(
            &entity_type,
            &entity_id,
            action,
            &to_state,
            &fields,
            &mut chain,
        );

        Ok(result)
    }

    /// Recursively dispatch reactions. `chain` holds the steps the cascade is
    /// inside of, oldest first; a reaction that would repeat one is not run
    /// and is reported as a failed result. Simulation targets are registered
    /// actors, never made by the chain, so every step is a found entity.
    fn dispatch_cascade(
        &mut self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        to_state: &str,
        fields: &serde_json::Value,
        chain: &mut Vec<ChainStep>,
    ) {
        // The chain starts at the action that set the cascade off.
        let depth = u32::try_from(chain.len().saturating_sub(1)).unwrap_or(u32::MAX);
        // Clone the matching rules to avoid borrow conflict
        let rules: Vec<_> = self
            .registry
            .lookup(&self.tenant, entity_type, action, to_state)
            .into_iter()
            .cloned()
            .collect();

        // Guards and args read the source record as every guard does: a
        // variable it stores no value for reads as its declared initial value.
        let read = match self.declared.get(entity_type) {
            Some(declared) => declared.fields_as_read(fields),
            None => fields.clone(),
        };

        for rule in rules {
            // Guard evaluation: skip rules whose guard fails. Guard-skipped
            // rules do not emit a `ReactionResult` — they never fired.
            if let Some(guard) = &rule.when.guard {
                let mut related = temper_jit::table::RelatedMap::new();
                for (reference, ids) in super::guard::related_ids(guard, &read) {
                    let statuses = ids
                        .iter()
                        .map(|id| {
                            self.entity_to_actor
                                .get(&format!("{}:{id}", reference.0))
                                .map(|actor_id| self.inner.status(actor_id))
                        })
                        .collect();
                    related.insert(reference, temper_jit::table::Related::Statuses(statuses));
                }
                if !super::guard::guard_holds(guard, &read, to_state, &related) {
                    continue;
                }
            }

            let target_entity_id =
                match super::resolver::resolve_target_id(&rule.resolve_target, entity_id, fields) {
                    Some(id) => id,
                    None => {
                        self.last_results.push(ReactionResult {
                            rule_name: rule.name.clone(),
                            success: false,
                            target_status: None,
                            error: Some("Could not resolve target entity ID".to_string()),
                            depth,
                        });
                        continue;
                    }
                };
            let step = found_step(&rule.then.entity_type, &target_entity_id, &rule.then.action);
            if let Some(repeat) = RepeatedStep::find(chain, &step) {
                self.last_results.push(ReactionResult::stopped_on_repeat(
                    &rule.name, depth, &repeat,
                ));
                continue;
            }

            // Find the actor for the target entity
            let target_key = format!("{}:{}", rule.then.entity_type, target_entity_id);
            let target_actor_id = match self.entity_to_actor.get(&target_key) {
                Some(id) => id.clone(),
                None => {
                    self.last_results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: false,
                        target_status: None,
                        error: Some(format!("No actor found for {target_key}")),
                        depth,
                    });
                    continue;
                }
            };

            // Execute the target action
            let effective_params =
                super::params::build_effective_params(&rule.then, entity_id, &read, &rule.name);
            let params_str = match serde_json::to_string(&effective_params) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        rule = %rule.name,
                        error = %e,
                        "failed to serialize reaction params, skipping rule"
                    );
                    self.last_results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: false,
                        target_status: None,
                        error: Some(format!("param serialization failed: {e}")),
                        depth,
                    });
                    continue;
                }
            };
            let step_result = self
                .inner
                .step(&target_actor_id, &rule.then.action, &params_str);

            match step_result {
                Ok(result_fields) => {
                    let target_status = self.inner.status(&target_actor_id);
                    self.last_results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: true,
                        target_status: Some(target_status.clone()),
                        error: None,
                        depth,
                    });

                    // Recurse: the target action may trigger further reactions.
                    // Propagate actual post-action fields so downstream resolvers
                    // (Field, CreateIfMissing) see real state, matching production
                    // dispatch behavior. A chain is as deep as its data, so each
                    // level runs on a stack that grows.
                    let target_type = rule.then.entity_type.clone();
                    let target_action = rule.then.action.clone();
                    chain.push(step);
                    crate::growable_stack::grow(|| {
                        self.dispatch_cascade(
                            &target_type,
                            &target_entity_id,
                            &target_action,
                            &target_status,
                            result_fields.get("fields").unwrap_or(&result_fields),
                            chain,
                        );
                    });
                    chain.pop();
                }
                Err(e) => {
                    self.last_results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: false,
                        target_status: None,
                        error: Some(e),
                        depth,
                    });
                }
            }
        }
    }

    /// Assert the status of an actor.
    pub fn assert_status(&self, actor_id: &str, expected: &str) {
        self.inner.assert_status(actor_id, expected);
    }

    /// Get the status of an actor.
    pub fn status(&self, actor_id: &str) -> String {
        self.inner.status(actor_id)
    }

    /// Get the reaction results from the last `step()` call.
    pub fn last_results(&self) -> &[ReactionResult] {
        &self.last_results
    }

    /// Check if any invariant violations occurred.
    pub fn has_violations(&self) -> bool {
        self.inner.has_violations()
    }
}

// Target resolver logic consolidated in super::resolver::resolve_target_id.
// Tests live in resolver.rs.

fn found_step(entity_type: &str, entity_id: &str, action: &str) -> ChainStep {
    ChainStep {
        entity_type: entity_type.to_string(),
        entity: StepEntity::Found(entity_id.to_string()),
        action: action.to_string(),
    }
}
