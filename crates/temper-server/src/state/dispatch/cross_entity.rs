mod reference;

use crate::request_context::AgentContext;
use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
use temper_runtime::tenant::TenantId;
use tracing::Instrument;

/// A collected `cross_entity_state` guard, flattened for runtime resolution:
/// `(target_type, id_source, required_statuses, forbidden_statuses, required_ref)`.
///
/// `required_statuses` is the allowlist (empty ⇒ unconstrained),
/// `forbidden_statuses` the denylist (empty ⇒ unconstrained), and `required_ref`
/// carries the IOA `required` attribute (ARN-92 #2).
type CrossGuardSpec = (String, String, Vec<String>, Vec<String>, bool);

impl crate::state::ServerState {
    pub(crate) async fn durable_reference_target_exists(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> bool {
        if let Some((store, _backend)) = self.event_journal() {
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let Ok(events) = store.read_events(&persistence_id, 0).await else {
                return false;
            };
            if events.is_empty()
                || events
                    .last()
                    .is_some_and(|event| event.event_type == "Deleted")
            {
                return false;
            }
        } else if !self.entity_exists(tenant, entity_type, entity_id) {
            return false;
        }
        self.ensure_entity_loaded(tenant, entity_type, entity_id)
            .await
    }

    pub(crate) async fn scoped_reference_target_exists(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        pin: &SchemaExecutionPin,
    ) -> bool {
        let Some((store, _)) = self.event_journal() else {
            let actor_key = format!(
                "{tenant}:{entity_type}:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    entity_id, pin,
                )
            );
            return self
                .actor_registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&actor_key);
        };
        let persistence_id = format!(
            "{tenant}:{entity_type}:{}",
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                entity_id, pin
            )
        );
        let Ok(events) = store.read_events(&persistence_id, 0).await else {
            return false;
        };
        !events.is_empty()
            && events
                .last()
                .is_none_or(|event| event.event_type != "Deleted")
    }

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
        schema_pin: Option<&SchemaExecutionPin>,
    ) -> std::collections::BTreeMap<String, bool> {
        use crate::entity_actor::effects::MAX_CROSS_ENTITY_LOOKUPS;

        let mut result = std::collections::BTreeMap::new();

        // Get the transition table to find cross-entity guards.
        let cross_guards: Vec<CrossGuardSpec> = {
            let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
            let table = match schema_pin {
                Some(pin) => registry.get_scoped_table_at_digest(
                    tenant,
                    &pin.scope,
                    &pin.bundle_digest,
                    entity_type,
                ),
                None => registry
                    .get_spec(tenant, entity_type)
                    .map(|spec| spec.table()),
            };
            let Some(table) = table else {
                return result;
            };

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
        let current_response = match schema_pin {
            Some(pin) => {
                self.get_scoped_entity_state(tenant, entity_type, entity_id, pin.clone())
                    .await
            }
            None => {
                self.get_tenant_entity_state(tenant, entity_type, entity_id)
                    .await
            }
        };
        let current_fields = match current_response {
            Ok(resp) => resp.state.fields,
            Err(_) => return result,
        };

        // A resolvable target status satisfies the guard iff it is allowed by
        // the allowlist (empty allowlist ⇒ unconstrained) AND not in the
        // denylist (empty denylist ⇒ unconstrained).
        let status_ok = |status: &str, required: &[String], forbidden: &[String]| -> bool {
            let allowed = required.is_empty() || required.iter().any(|s| s == status);
            let not_forbidden = !forbidden.iter().any(|s| s == status);
            allowed && not_forbidden
        };

        // Resolve each cross-entity guard (budget-limited)
        let mut lookup_count = 0;
        for (target_type, id_source, required_statuses, forbidden_statuses, required_ref) in
            &cross_guards
        {
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
                    // Empty list relation. A *required* relation cannot be
                    // satisfied by an absent target, so it fails the guard
                    // (ARN-92 #2). An optional list stays vacuous-true to
                    // preserve the existing blast radius.
                    result.insert(key, !*required_ref);
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
                    let status = match schema_pin {
                        Some(pin) => self
                            .get_scoped_entity_state(tenant, target_type, item_id, pin.clone())
                            .await
                            .ok()
                            .map(|response| response.state.status),
                        None => {
                            self.resolve_entity_status(tenant, target_type, item_id)
                                .await
                        }
                    };
                    if let Some(status) = status {
                        if !status_ok(&status, required_statuses, forbidden_statuses) {
                            all_matched = false;
                            break;
                        }
                    } else {
                        // A non-empty list element pointing at a missing entity
                        // cannot satisfy an allowlist; with a denylist-only
                        // guard the absent target is treated as not-forbidden
                        // (the container does not exist, so it cannot be in a
                        // bad state). A *required* ref still fails (the relation
                        // was declared mandatory).
                        if *required_ref || !required_statuses.is_empty() {
                            all_matched = false;
                            break;
                        }
                    }
                }
                result.insert(key, all_matched);
                continue;
            }

            // Scalar field: resolve a single entity ID.
            let target_id = field_value.and_then(|v| v.as_str()).unwrap_or("");

            if target_id.is_empty() {
                // Empty/missing scalar ref. A *required* ref that was never set
                // cannot satisfy a cross-entity status precondition, so it fails
                // the guard (ARN-92 #2). An optional ref stays vacuous-true.
                result.insert(key, !*required_ref);
                continue;
            }

            lookup_count += 1;
            let status = match schema_pin {
                Some(pin) => self
                    .get_scoped_entity_state(tenant, target_type, target_id, pin.clone())
                    .await
                    .ok()
                    .map(|response| response.state.status),
                None => {
                    self.resolve_entity_status(tenant, target_type, target_id)
                        .await
                }
            };
            if let Some(status) = status {
                result.insert(
                    key,
                    status_ok(&status, required_statuses, forbidden_statuses),
                );
            } else {
                // Non-empty scalar ref to a target that does not resolve. An
                // allowlist cannot be satisfied by an absent target, so it
                // fails; a *required* ref likewise fails (the relation was
                // declared mandatory). A denylist-only, non-required guard is
                // about a *specific bad state* the container can be in — an
                // absent container is not in any state, so it does not forbid
                // the action (matches the runtime write-gate's "unresolvable ⇒
                // allow" semantics, and the list-element-missing branch above).
                let allow = required_statuses.is_empty() && !*required_ref;
                result.insert(key, allow);
            }
        }

        result
    }

    /// Recursively collect CrossEntityStateIn guards from a guard tree into
    /// [`CrossGuardSpec`] tuples for runtime resolution.
    pub(super) fn collect_cross_guards(
        guard: &temper_jit::table::Guard,
        out: &mut Vec<CrossGuardSpec>,
    ) {
        use temper_jit::table::Guard;
        match guard {
            Guard::CrossEntityStateIn {
                entity_type,
                entity_id_source,
                required_status,
                forbidden_status,
                required,
            } => {
                out.push((
                    entity_type.clone(),
                    entity_id_source.clone(),
                    required_status.clone(),
                    forbidden_status.clone(),
                    *required,
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
            let workflow_root_entity_type = agent
                .workflow_root_entity_type
                .clone()
                .unwrap_or_else(|| parent_t.clone());
            let workflow_root_entity_id = agent
                .workflow_root_entity_id
                .clone()
                .unwrap_or_else(|| parent_i.clone());
            let workflow_run_id = agent
                .workflow_run_id
                .clone()
                .unwrap_or_else(|| format!("{parent_t}:{parent_i}"));
            let span = tracing::info_span!(
                "dispatch.background_spawn_entity",
                workflow.root_entity_type = %workflow_root_entity_type,
                workflow.root_entity_id = %workflow_root_entity_id,
                workflow.run_id = %workflow_run_id,
                parent_type = %parent_t,
                parent_id = %parent_i,
                child_type = %child_type,
                child_id = %child_id,
            );

            tokio::spawn(
                async move {
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
                }
                .instrument(span),
            );
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

#[cfg(test)]
#[path = "cross_entity_test.rs"]
mod required_ref_tests;
