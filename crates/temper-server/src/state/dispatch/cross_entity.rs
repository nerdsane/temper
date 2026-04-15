use crate::request_context::AgentContext;
use temper_runtime::tenant::TenantId;

impl crate::state::ServerState {
    /// Pre-resolve cross-entity state guards for an action.
    ///
    /// Reads the TransitionTable, walks rules for the given action, and for each
    /// `CrossEntityStateIn` guard, resolves the target entity's status and compares
    /// against the required statuses.
    pub(super) async fn resolve_cross_entity_guards(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
    ) -> std::collections::BTreeMap<String, bool> {
        use crate::entity_actor::effects::MAX_CROSS_ENTITY_LOOKUPS;

        let mut result = std::collections::BTreeMap::new();

        // Get the transition table to find cross-entity guards
        let cross_guards: Vec<(String, String, Vec<String>)> = {
            let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
            let Some(spec) = registry.get_spec(tenant, entity_type) else {
                return result;
            };
            let table = spec.table();

            // Collect CrossEntityStateIn guards from rules matching this action
            let mut guards = Vec::new();
            for rule in &table.rules {
                if rule.name == action {
                    Self::collect_cross_guards(&rule.guard, &mut guards);
                }
            }
            guards
        };

        if cross_guards.is_empty() {
            return result;
        }

        // Get current entity fields to resolve target entity IDs
        let current_fields = match self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            Ok(resp) => resp.state.fields,
            Err(_) => return result,
        };

        // Resolve each cross-entity guard (budget-limited)
        let mut lookup_count = 0;
        for (target_type, id_source, required_statuses) in &cross_guards {
            if lookup_count >= MAX_CROSS_ENTITY_LOOKUPS {
                tracing::warn!(
                    entity_type,
                    entity_id,
                    "cross-entity lookup budget exhausted ({})",
                    MAX_CROSS_ENTITY_LOOKUPS
                );
                break;
            }

            let field_value = current_fields.get(id_source);
            let key = format!("__xref:{}:{}", target_type, id_source);

            // If the field is a list (e.g. child_agent_ids), resolve each element.
            if let Some(arr) = field_value.and_then(|v| v.as_array()) {
                if arr.is_empty() {
                    // Empty list: vacuous truth — guard passes.
                    result.insert(key, true);
                    continue;
                }
                let mut all_matched = true;
                for item in arr {
                    let item_id = item.as_str().unwrap_or("");
                    if item_id.is_empty() {
                        continue;
                    }
                    lookup_count += 1;
                    if lookup_count > MAX_CROSS_ENTITY_LOOKUPS {
                        tracing::warn!(
                            entity_type,
                            entity_id,
                            "cross-entity lookup budget exhausted ({})",
                            MAX_CROSS_ENTITY_LOOKUPS
                        );
                        all_matched = false;
                        break;
                    }
                    if let Some(status) = self
                        .resolve_entity_status(tenant, target_type, item_id)
                        .await
                    {
                        if !required_statuses.iter().any(|s| s == &status) {
                            all_matched = false;
                            break;
                        }
                    } else {
                        all_matched = false;
                        break;
                    }
                }
                result.insert(key, all_matched);
                continue;
            }

            // Scalar field: resolve a single entity ID.
            let target_id = field_value.and_then(|v| v.as_str()).unwrap_or("");

            if target_id.is_empty() {
                // Empty string: vacuous truth — guard passes.
                result.insert(key, true);
                continue;
            }

            lookup_count += 1;
            if let Some(status) = self
                .resolve_entity_status(tenant, target_type, target_id)
                .await
            {
                let matched = required_statuses.iter().any(|s| s == &status);
                result.insert(key, matched);
            } else {
                result.insert(key, false);
            }
        }

        result
    }

    /// Recursively collect CrossEntityStateIn guards from a guard tree.
    pub(super) fn collect_cross_guards(
        guard: &temper_jit::table::Guard,
        out: &mut Vec<(String, String, Vec<String>)>,
    ) {
        use temper_jit::table::Guard;
        match guard {
            Guard::CrossEntityStateIn {
                entity_type,
                entity_id_source,
                required_status,
            } => {
                out.push((
                    entity_type.clone(),
                    entity_id_source.clone(),
                    required_status.clone(),
                ));
            }
            Guard::And(guards) => {
                for g in guards {
                    Self::collect_cross_guards(g, out);
                }
            }
            _ => {}
        }
    }

    /// Dispatch entity spawn requests post-transition.
    ///
    /// This is a **sync** method (like `dispatch_scheduled_actions`) so that
    /// `tokio::spawn` inside it does not cause async recursion.
    /// Creates child entities and optionally dispatches initial actions.
    pub(super) fn dispatch_spawn_requests(
        &self,
        tenant: &TenantId,
        parent_type: &str,
        parent_id: &str,
        spawn_requests: &[crate::entity_actor::effects::SpawnRequest],
        action_params: &serde_json::Value,
        agent_ctx: &AgentContext,
    ) {
        use crate::entity_actor::effects::MAX_SPAWNS_PER_TRANSITION;

        for (spawn_count, req) in spawn_requests.iter().enumerate() {
            if spawn_count >= MAX_SPAWNS_PER_TRANSITION {
                tracing::warn!(
                    parent_type,
                    parent_id,
                    "spawn budget exhausted ({})",
                    MAX_SPAWNS_PER_TRANSITION
                );
                break;
            }

            let state = self.clone();
            let t = tenant.clone();
            let parent_t = parent_type.to_string();
            let parent_i = parent_id.to_string();
            let child_type = req.entity_type.clone();
            let child_id = req.entity_id.clone();
            let initial_action = req.initial_action.clone();
            let parent_params = action_params.clone();
            let agent = agent_ctx.clone();
            let copied_fields = req.copied_field_values.clone();

            tokio::spawn(async move {
                // determinism-ok: spawn dispatch is a background side-effect
                let mut parent_fields = serde_json::Map::new();
                parent_fields.insert(
                    "parent_type".to_string(),
                    serde_json::Value::String(parent_t.clone()),
                );
                parent_fields.insert(
                    "parent_id".to_string(),
                    serde_json::Value::String(parent_i.clone()),
                );
                parent_fields.insert(
                    format!("{}_id", to_snake_case(&parent_t)),
                    serde_json::Value::String(parent_i.clone()),
                );
                let initial_fields = serde_json::Value::Object(parent_fields.clone());

                match state
                    .get_or_create_tenant_entity(&t, &child_type, &child_id, initial_fields)
                    .await
                {
                    Ok(_) => {
                        tracing::info!(
                            parent_type = %parent_t,
                            parent_id = %parent_i,
                            child_type = %child_type,
                            child_id = %child_id,
                            "spawned child entity"
                        );

                        if let Some(action) = initial_action {
                            let mut initial_action_params =
                                parent_params.as_object().cloned().unwrap_or_default();
                            for (key, value) in parent_fields {
                                initial_action_params.insert(key, value);
                            }
                            // Merge copied field values (take precedence over parent params)
                            for (key, value) in &copied_fields {
                                initial_action_params.insert(key.clone(), value.clone());
                            }
                            if let Err(e) = state
                                .dispatch_tenant_action(
                                    &t,
                                    &child_type,
                                    &child_id,
                                    &action,
                                    serde_json::Value::Object(initial_action_params),
                                    &agent,
                                )
                                .await
                            {
                                tracing::error!(
                                    child_type = %child_type,
                                    child_id = %child_id,
                                    action = %action,
                                    error = %e,
                                    "failed to dispatch initial action on spawned entity"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            child_type = %child_type,
                            child_id = %child_id,
                            error = %e,
                            "failed to spawn child entity"
                        );
                    }
                }
            });
        }
    }
}

fn to_snake_case(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for (index, ch) in value.chars().enumerate() {
        match ch {
            'A'..='Z' => {
                if index > 0 {
                    result.push('_');
                }
                result.push(ch.to_ascii_lowercase());
            }
            '-' | ' ' => result.push('_'),
            _ => result.push(ch.to_ascii_lowercase()),
        }
    }
    result
}
