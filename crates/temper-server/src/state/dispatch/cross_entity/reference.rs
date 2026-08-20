//! Typed-reference evidence resolution for prospective writes.

use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
use temper_runtime::tenant::TenantId;

impl crate::state::ServerState {
    /// Resolve durable same-tenant existence evidence for every typed reference
    /// that can be observed by the prospective write.
    pub(crate) async fn resolve_reference_evidence(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: Option<&str>,
        incoming: &serde_json::Value,
        schema_pin: Option<&SchemaExecutionPin>,
    ) -> std::collections::BTreeMap<String, bool> {
        use crate::entity_actor::reference_contract::target_evidence_key;

        let table = {
            let registry = self
                .registry
                .read()
                .expect("spec registry lock should not be poisoned");
            match schema_pin {
                Some(pin) => registry.get_scoped_table_at_digest(
                    tenant,
                    &pin.scope,
                    &pin.bundle_digest,
                    entity_type,
                ),
                None => registry
                    .get_spec(tenant, entity_type)
                    .map(|spec| spec.table()),
            }
        };
        let table = if schema_pin.is_none() {
            table.or_else(|| self.transition_tables.get(entity_type).cloned())
        } else {
            table
        };
        let Some(table) = table else {
            return std::collections::BTreeMap::new();
        };
        let (state_refs, action_refs) = {
            let state_refs = table
                .state_var_metadata
                .iter()
                .filter(|(_, metadata)| metadata.var_type.as_deref() == Some("ref"))
                .filter_map(|(name, metadata)| {
                    metadata
                        .entity_type
                        .as_ref()
                        .map(|target| (name.clone(), target.clone()))
                })
                .collect::<Vec<_>>();
            let action_refs = action
                .and_then(|name| table.action_params.get(name))
                .into_iter()
                .flatten()
                .filter(|(_, metadata)| metadata.param_type == "ref")
                .filter_map(|(name, metadata)| {
                    metadata
                        .entity_type
                        .as_ref()
                        .map(|target| (name.clone(), target.clone()))
                })
                .collect::<Vec<_>>();
            (state_refs, action_refs)
        };

        if state_refs.is_empty() && action_refs.is_empty() {
            return std::collections::BTreeMap::new();
        }
        let current = match schema_pin {
            Some(pin) => self
                .get_scoped_entity_state(tenant, entity_type, entity_id, pin.clone())
                .await
                .ok()
                .map(|response| response.state.fields)
                .unwrap_or_else(|| serde_json::json!({})),
            None if self
                .ensure_entity_loaded(tenant, entity_type, entity_id)
                .await =>
            {
                self.get_tenant_entity_state(tenant, entity_type, entity_id)
                    .await
                    .ok()
                    .map(|response| response.state.fields)
                    .unwrap_or_else(|| serde_json::json!({}))
            }
            None => serde_json::json!({}),
        };
        let mut targets = Vec::new();
        for (name, target_type) in state_refs {
            let value = reference_field(incoming, &name)
                .or_else(|| reference_field(&current, &name))
                .and_then(serde_json::Value::as_str);
            if let Some(target_id) = value.filter(|value| !value.is_empty()) {
                targets.push((target_type, target_id.to_string()));
            }
        }
        for (name, target_type) in action_refs {
            if let Some(target_id) = reference_field(incoming, &name)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
            {
                targets.push((target_type, target_id.to_string()));
            }
        }
        targets.sort();
        targets.dedup();
        debug_assert!(
            targets.len() <= temper_spec::automaton::MAX_REFERENCE_TARGETS_PER_WRITE,
            "verified reference declaration exceeded its target lookup budget"
        );
        let mut evidence = std::collections::BTreeMap::new();
        for (target_type, target_id) in targets {
            let exists = match schema_pin {
                Some(pin) => {
                    self.scoped_reference_target_exists(tenant, &target_type, &target_id, pin)
                        .await
                }
                None => {
                    self.durable_reference_target_exists(tenant, &target_type, &target_id)
                        .await
                }
            };
            evidence.insert(target_evidence_key(&target_type, &target_id), exists);
        }
        evidence
    }
}

fn reference_field<'a>(fields: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    fields
        .get(name)
        .or_else(|| fields.get(temper_spec::to_snake_case(name)))
        .or_else(|| fields.get(temper_spec::to_pascal_case(name)))
}
