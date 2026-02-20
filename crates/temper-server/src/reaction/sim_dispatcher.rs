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
use super::types::{MAX_REACTION_DEPTH, ReactionResult, TargetResolver};

/// Maps actor IDs to their (entity_type, entity_id) for target resolution.
struct ActorMeta {
    entity_type: String,
    entity_id: String,
}

/// Deterministic simulation system with reaction cascade support.
///
/// Wraps a real [`SimActorSystem`] and dispatches reaction rules synchronously
/// after each successful `step()`. The cascade is bounded by [`MAX_REACTION_DEPTH`].
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
        let fields = result.clone();

        // Dispatch reactions recursively
        self.dispatch_cascade(&entity_type, &entity_id, action, &to_state, &fields, 0);

        Ok(result)
    }

    /// Recursively dispatch reactions, bounded by MAX_REACTION_DEPTH.
    fn dispatch_cascade(
        &mut self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        to_state: &str,
        fields: &serde_json::Value,
        depth: u32,
    ) {
        if depth >= MAX_REACTION_DEPTH {
            return;
        }

        // Clone the matching rules to avoid borrow conflict
        let rules: Vec<_> = self
            .registry
            .lookup(&self.tenant, entity_type, action, to_state)
            .into_iter()
            .cloned()
            .collect();

        for rule in rules {
            let target_entity_id = match resolve_target_id(&rule.resolve_target, entity_id, fields)
            {
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
            let params_str = serde_json::to_string(&rule.then.params).unwrap_or_default();
            let step_result = self
                .inner
                .step(&target_actor_id, &rule.then.action, &params_str);

            match step_result {
                Ok(_) => {
                    let target_status = self.inner.status(&target_actor_id);
                    self.last_results.push(ReactionResult {
                        rule_name: rule.name.clone(),
                        success: true,
                        target_status: Some(target_status.clone()),
                        error: None,
                        depth,
                    });

                    // Recurse: the target action may trigger further reactions
                    let target_type = rule.then.entity_type.clone();
                    let target_action = rule.then.action.clone();
                    let empty_fields = serde_json::json!({});
                    self.dispatch_cascade(
                        &target_type,
                        &target_entity_id,
                        &target_action,
                        &target_status,
                        &empty_fields,
                        depth + 1,
                    );
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

/// Resolve the target entity ID from a [`TargetResolver`].
fn resolve_target_id(
    resolver: &TargetResolver,
    source_entity_id: &str,
    fields: &serde_json::Value,
) -> Option<String> {
    match resolver {
        TargetResolver::Field { field } => fields
            .get(field)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        TargetResolver::SameId => Some(source_entity_id.to_string()),
        TargetResolver::Static { entity_id } => Some(entity_id.clone()),
        TargetResolver::CreateIfMissing { id_field } => {
            // Try to read from fields; if missing, derive deterministically
            fields
                .get(id_field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| Some(format!("{source_entity_id}-derived")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_target_field() {
        let fields = serde_json::json!({"payment_id": "pay-1"});
        let resolver = TargetResolver::Field {
            field: "payment_id".to_string(),
        };
        assert_eq!(
            resolve_target_id(&resolver, "order-1", &fields),
            Some("pay-1".to_string())
        );
    }

    #[test]
    fn resolve_target_field_missing() {
        let fields = serde_json::json!({});
        let resolver = TargetResolver::Field {
            field: "payment_id".to_string(),
        };
        assert_eq!(resolve_target_id(&resolver, "order-1", &fields), None);
    }

    #[test]
    fn resolve_target_same_id() {
        let resolver = TargetResolver::SameId;
        assert_eq!(
            resolve_target_id(&resolver, "order-1", &serde_json::json!({})),
            Some("order-1".to_string())
        );
    }

    #[test]
    fn resolve_target_static() {
        let resolver = TargetResolver::Static {
            entity_id: "singleton".to_string(),
        };
        assert_eq!(
            resolve_target_id(&resolver, "order-1", &serde_json::json!({})),
            Some("singleton".to_string())
        );
    }

    #[test]
    fn resolve_target_create_if_missing_with_field() {
        let fields = serde_json::json!({"b_id": "existing-id"});
        let resolver = TargetResolver::CreateIfMissing {
            id_field: "b_id".to_string(),
        };
        assert_eq!(
            resolve_target_id(&resolver, "order-1", &fields),
            Some("existing-id".to_string())
        );
    }

    #[test]
    fn resolve_target_create_if_missing_derives() {
        let fields = serde_json::json!({});
        let resolver = TargetResolver::CreateIfMissing {
            id_field: "b_id".to_string(),
        };
        assert_eq!(
            resolve_target_id(&resolver, "order-1", &fields),
            Some("order-1-derived".to_string())
        );
    }
}
