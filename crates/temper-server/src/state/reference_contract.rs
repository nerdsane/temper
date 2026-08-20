//! Server-state orchestration for ADR-0156 creation and activation audits.

use std::collections::BTreeMap;

use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;

use super::ServerState;
use crate::entity_actor::EntityState;

impl ServerState {
    /// Audit existing entities before activating a newly submitted reference contract.
    pub async fn audit_reference_contract_activation(
        &self,
        tenant: &TenantId,
        ioa_sources: &BTreeMap<String, String>,
        entity_budget: usize,
    ) -> Result<(), String> {
        let mut audited = 0usize;
        for (entity_type, source) in ioa_sources {
            let table = TransitionTable::try_from_ioa_source(source)?;
            let contracted = table
                .state_var_metadata
                .values()
                .any(|metadata| metadata.var_type.as_deref() == Some("ref"))
                || table.keys.iter().any(|key| key.entity_id);
            if !contracted {
                continue;
            }
            let remaining = entity_budget.saturating_sub(audited);
            let ids = self
                .list_entity_ids_bounded(tenant, entity_type, remaining)
                .ok_or_else(|| incomplete_audit(entity_type, entity_budget))?;
            for entity_id in ids {
                audited = audited.saturating_add(1);
                if audited > entity_budget {
                    return Err(incomplete_audit(entity_type, entity_budget));
                }
                self.audit_reference_entity(tenant, entity_type, &entity_id, &table)
                    .await?;
            }
        }
        Ok(())
    }

    async fn audit_reference_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        table: &TransitionTable,
    ) -> Result<(), String> {
        let response = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await?;
        let mut evidence = BTreeMap::new();
        let historical_events = if let Some((store, _backend)) = self.event_journal() {
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            store.read_events(&persistence_id, 0).await.map_err(|error| {
                format!(
                    "ReferenceContractActivationAuditIncomplete:entity={entity_type}:id={entity_id}:journal={error}"
                )
            })?
            .into_iter()
            .map(|envelope| {
                serde_json::from_value::<crate::entity_actor::EntityEvent>(envelope.payload)
                    .map_err(|error| {
                        format!(
                            "ReferenceContractActivationAuditIncomplete:entity={entity_type}:id={entity_id}:event_decode={error}"
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
        } else {
            response.state.events.iter().cloned().collect()
        };
        for (name, metadata) in &table.state_var_metadata {
            if metadata.var_type.as_deref() != Some("ref") {
                continue;
            }
            let reference = reference_field(&response.state.fields, name).map_err(|()| {
                format!(
                    "ReferenceContractActivationAuditFailed:entity={entity_type}:id={entity_id}:field={name}:conflicting_alias_values"
                )
            })?;
            if let Some(target_id) = reference
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
            {
                let target_type = metadata.entity_type.as_deref().unwrap_or_default();
                let exists = self
                    .durable_reference_target_exists(tenant, target_type, target_id)
                    .await;
                evidence.insert(
                    crate::entity_actor::reference_contract::target_evidence_key(
                        target_type,
                        target_id,
                    ),
                    exists,
                );
            }
        }
        crate::entity_actor::reference_contract::validate_prospective_state(
            table,
            "ActivationAudit",
            &response.state,
            &response.state,
            &evidence,
        )
        .map_err(|error| error.to_string())?;
        for (name, metadata) in &table.state_var_metadata {
            if metadata.var_type.as_deref() != Some("ref") {
                continue;
            }
            let mut observed: Option<String> = None;
            for event in &historical_events {
                match reference_assignment(event, name) {
                    HistoricalAssignment::Set(value) => {
                        if observed.as_deref().is_some_and(|prior| prior != value) {
                            return Err(format!(
                                "ReferenceContractActivationAuditFailed:entity={entity_type}:id={entity_id}:field={name}:historical_rebind"
                            ));
                        }
                        observed = Some(value.to_string());
                    }
                    HistoricalAssignment::Clear if observed.is_some() => {
                        return Err(format!(
                            "ReferenceContractActivationAuditFailed:entity={entity_type}:id={entity_id}:field={name}:historical_clear"
                        ));
                    }
                    HistoricalAssignment::Invalid => {
                        return Err(format!(
                            "ReferenceContractActivationAuditFailed:entity={entity_type}:id={entity_id}:field={name}:historical_invalid_value"
                        ));
                    }
                    HistoricalAssignment::NoWrite | HistoricalAssignment::Clear => {}
                }
            }
        }
        Ok(())
    }

    /// Derive and validate a create request before any actor is routed or spawned.
    pub async fn prepare_reference_contract_create(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        supplied_id: Option<&str>,
        initial_fields: &serde_json::Value,
    ) -> Result<Option<String>, String> {
        self.prepare_reference_contract_create_with_pin(
            tenant,
            entity_type,
            supplied_id,
            initial_fields,
            None,
        )
        .await
    }

    /// Validate a create against one exact immutable scoped schema.
    pub async fn prepare_scoped_reference_contract_create(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        supplied_id: Option<&str>,
        initial_fields: &serde_json::Value,
        schema_pin: &temper_runtime::persistence::schema_deployment::SchemaExecutionPin,
    ) -> Result<Option<String>, String> {
        self.prepare_reference_contract_create_with_pin(
            tenant,
            entity_type,
            supplied_id,
            initial_fields,
            Some(schema_pin),
        )
        .await
    }

    async fn prepare_reference_contract_create_with_pin(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        supplied_id: Option<&str>,
        initial_fields: &serde_json::Value,
        schema_pin: Option<&temper_runtime::persistence::schema_deployment::SchemaExecutionPin>,
    ) -> Result<Option<String>, String> {
        let table = {
            let registry = self
                .registry
                .read()
                .expect("spec registry lock should not be poisoned");
            match schema_pin {
                Some(pin) => registry
                    .get_scoped_table_at_digest(tenant, &pin.scope, &pin.bundle_digest, entity_type)
                    .map(|table| (*table).clone()),
                None => registry
                    .get_table_live(tenant, entity_type)
                    .map(|table| table.read().expect("table lock poisoned").clone()),
            }
        }
        .or_else(|| {
            schema_pin
                .is_none()
                .then(|| {
                    self.transition_tables
                        .get(entity_type)
                        .map(|table| (**table).clone())
                })
                .flatten()
        });
        let Some(table) = table else {
            return Ok(supplied_id.map(str::to_string));
        };
        let fields = initial_fields
            .as_object()
            .ok_or_else(|| "InvalidReferenceValue:create body must be an object".to_string())?;
        let entity_id = crate::entity_actor::reference_contract::derive_or_validate_entity_id(
            &table,
            supplied_id,
            fields,
            "Create",
        )
        .map_err(|error| error.to_string())?;
        let Some(entity_id) = entity_id else {
            return Ok(None);
        };
        let evidence = self
            .resolve_reference_evidence(
                tenant,
                entity_type,
                &entity_id,
                None,
                initial_fields,
                schema_pin,
            )
            .await;
        let current = empty_entity(entity_type, &entity_id, &table.initial_state);
        let mut prospective = current.clone();
        prospective.fields = initial_fields.clone();
        if let Some(object) = prospective.fields.as_object_mut() {
            object.insert("Id".into(), serde_json::Value::String(entity_id.clone()));
            object.insert(
                "Status".into(),
                serde_json::Value::String(table.initial_state.clone()),
            );
        }
        crate::entity_actor::reference_contract::validate_prospective_state(
            &table,
            "Create",
            &current,
            &prospective,
            &evidence,
        )
        .map_err(|error| error.to_string())?;
        Ok(Some(entity_id))
    }
}

enum HistoricalAssignment<'a> {
    NoWrite,
    Clear,
    Set(&'a str),
    Invalid,
}

fn reference_assignment<'a>(
    event: &'a crate::entity_actor::EntityEvent,
    name: &str,
) -> HistoricalAssignment<'a> {
    let field_update = event.action == crate::entity_actor::types::FIELD_UPDATE_EVENT_TYPE;
    let value = if field_update {
        event
            .params
            .get("fields")
            .map(|fields| reference_field(fields, name))
            .transpose()
            .map(|value| value.flatten())
    } else {
        reference_field(&event.params, name)
    };
    let Ok(value) = value else {
        return HistoricalAssignment::Invalid;
    };
    match value {
        Some(serde_json::Value::String(value)) if !value.is_empty() => {
            HistoricalAssignment::Set(value)
        }
        Some(serde_json::Value::String(_)) | Some(serde_json::Value::Null) => {
            HistoricalAssignment::Clear
        }
        Some(_) => HistoricalAssignment::Invalid,
        None if field_update
            && event
                .params
                .get("replace")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false) =>
        {
            HistoricalAssignment::Clear
        }
        None => HistoricalAssignment::NoWrite,
    }
}

fn reference_field<'a>(
    fields: &'a serde_json::Value,
    name: &str,
) -> Result<Option<&'a serde_json::Value>, ()> {
    let Some(fields) = fields.as_object() else {
        return Ok(None);
    };
    let canonical = temper_spec::to_snake_case(name);
    let mut matches = fields
        .iter()
        .filter(|(candidate, _)| temper_spec::to_snake_case(candidate) == canonical);
    let Some((_, first)) = matches.next() else {
        return Ok(None);
    };
    if matches.any(|(_, value)| value != first) {
        return Err(());
    }
    Ok(Some(first))
}

fn incomplete_audit(entity_type: &str, entity_budget: usize) -> String {
    format!(
        "ReferenceContractActivationAuditIncomplete:entity={entity_type}:budget={entity_budget}"
    )
}

fn empty_entity(entity_type: &str, entity_id: &str, initial_state: &str) -> EntityState {
    EntityState {
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
        status: initial_state.to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({}),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    }
}
