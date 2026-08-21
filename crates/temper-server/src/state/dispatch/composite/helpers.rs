use super::*;

#[derive(Debug, Clone)]
pub(super) struct CompositeCreateAuthDefaults {
    pub(super) initial_state: String,
    pub(super) has_spec: bool,
}

pub(super) fn empty_params() -> Value {
    Value::Object(Default::default())
}

pub(super) fn synthetic_initial_state(
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
) -> EntityState {
    EntityState {
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
        status: table.initial_state.clone(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({ "Id": entity_id }),
        events: Default::default(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    }
}

pub(super) fn has_sub_writes(params: &Value) -> bool {
    params.get("sub_writes").is_some() || params.get("SubWrites").is_some()
}

pub(super) fn parse_sub_writes(params: &Value) -> Result<Vec<CompositeSubWrite>, DispatchError> {
    let raw = params
        .get("sub_writes")
        .or_else(|| params.get("SubWrites"))
        .ok_or_else(|| {
            DispatchError::Internal(
                "Composite integration result must include `sub_writes`/`SubWrites`".to_string(),
            )
        })?;

    serde_json::from_value(raw.clone())
        .map_err(|e| DispatchError::Internal(format!("Invalid composite sub_writes payload: {e}")))
}

pub(super) fn validate_sub_writes(
    metadata: &CompositeActionMetadata,
    sub_writes: &[CompositeSubWrite],
) -> Result<(), DispatchError> {
    if !metadata.sub_writes.is_empty() && sub_writes.is_empty() {
        return Err(DispatchError::Internal(
            "Composite action declared sub-writes but none were provided".to_string(),
        ));
    }

    let declared: BTreeSet<(String, String)> = metadata
        .sub_writes
        .iter()
        .map(|spec| (spec.target_entity.clone(), spec.action.clone()))
        .collect();

    for sub_write in sub_writes {
        if !declared.contains(&(sub_write.entity_type.clone(), sub_write.action.clone())) {
            return Err(DispatchError::Internal(format!(
                "Composite sub-write {}.{} is not declared by the action contract",
                sub_write.entity_type, sub_write.action
            )));
        }
    }

    Ok(())
}

pub(super) fn table_has_cross_entity_guards_for_action(
    table: &TransitionTable,
    action: &str,
) -> bool {
    for rule in &table.rules {
        if rule.name != action {
            continue;
        }
        let mut guards = Vec::new();
        crate::state::ServerState::collect_cross_guards(&rule.guard, &mut guards);
        if !guards.is_empty() {
            return true;
        }
    }
    false
}

pub(super) fn composite_sub_write_uses_parent_gate(
    metadata: &CompositeActionMetadata,
    entity_type: &str,
    action: &str,
) -> bool {
    if metadata.cedar_gate.is_none() {
        return false;
    }

    metadata.sub_writes.iter().any(|spec| {
        if spec.target_entity != entity_type || spec.action != action {
            return false;
        }

        matches!(
            (entity_type, action, spec.generated_from.as_deref()),
            (
                "Blob" | "Tree" | "Commit" | "Tag",
                "Create",
                Some("pack_bytes")
            ) | ("Ref", "Create" | "Update" | "Delete", Some("ref_updates"))
        )
    })
}

pub(super) fn should_skip_existing_pack_object_create(
    write: &PreparedCompositeSubWrite,
    stream: &AtomicCompositeStream,
) -> bool {
    write.uses_parent_gate
        && write.action == "Create"
        && stream.target_existed
        && is_pack_object_entity(&write.entity_type)
        && has_complete_git_object_payload(&stream.state.fields)
}

pub(super) fn is_incomplete_existing_pack_object_create(
    write: &PreparedCompositeSubWrite,
    stream: &AtomicCompositeStream,
) -> bool {
    write.uses_parent_gate
        && write.action == "Create"
        && stream.target_existed
        && is_pack_object_entity(&write.entity_type)
        && !has_complete_git_object_payload(&stream.state.fields)
}

pub(super) fn is_pack_object_entity(entity_type: &str) -> bool {
    matches!(entity_type, "Blob" | "Tree" | "Commit" | "Tag")
}

pub(super) fn validate_composite_ref_compare_and_set(
    parent_entity_type: &str,
    parent_action: &str,
    write: &PreparedCompositeSubWrite,
    stream: &AtomicCompositeStream,
) -> Result<(), DispatchError> {
    validate_ref_sub_write_compare_and_set(
        parent_entity_type,
        parent_action,
        write,
        stream.target_existed,
        &stream.state,
    )
}

pub(super) fn validate_composite_ref_preflight_compare_and_set(
    parent_entity_type: &str,
    parent_action: &str,
    write: &PreparedCompositeSubWrite,
    target: &PreflightCompositeTarget,
) -> Result<(), DispatchError> {
    validate_ref_sub_write_compare_and_set(
        parent_entity_type,
        parent_action,
        write,
        target.target_existed,
        &target.state,
    )
}

pub(super) fn validate_ref_sub_write_compare_and_set(
    parent_entity_type: &str,
    parent_action: &str,
    write: &PreparedCompositeSubWrite,
    target_existed: bool,
    state: &EntityState,
) -> Result<(), DispatchError> {
    if write.entity_type != "Ref"
        || !matches!(write.action.as_str(), "Create" | "Update" | "Delete")
    {
        return Ok(());
    }

    let Some(expected) = json_string_field(&write.params, "PreviousCommitSha") else {
        return Ok(());
    };

    let current = current_ref_target(target_existed, state);
    let expected_missing = is_zero_git_sha(&expected);
    let valid = if expected_missing {
        current.is_none() || current.is_some_and(is_zero_git_sha)
    } else {
        current == Some(expected.as_str())
    };
    if valid {
        return Ok(());
    }

    let found = current.unwrap_or("missing ref");
    Err(DispatchError::Conflict(format!(
        "composite {parent_entity_type}.{parent_action} sub-write {} stale ref {}: expected {}, found {}",
        write.idx, write.entity_id, expected, found
    )))
}

pub(super) fn current_ref_target(target_existed: bool, state: &EntityState) -> Option<&str> {
    if !target_existed || state.status == "Deleted" {
        return None;
    }
    state
        .fields
        .get("TargetCommitSha")
        .and_then(Value::as_str)
        .filter(|sha| !sha.is_empty())
}

pub(super) fn json_string_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

pub(super) fn is_zero_git_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte == b'0')
}

pub(super) fn has_complete_git_object_payload(fields: &Value) -> bool {
    fields
        .as_object()
        .and_then(|fields| fields.get("CanonicalBytes"))
        .is_some_and(non_empty_json_value)
}

pub(super) fn non_empty_json_value(value: &Value) -> bool {
    match value {
        Value::String(value) => !value.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Null => false,
        Value::Bool(_) | Value::Number(_) => true,
    }
}

pub(super) fn composite_create_resource_attrs_from_defaults(
    entity_id: &str,
    params: &Value,
    defaults: &CompositeCreateAuthDefaults,
) -> BTreeMap<String, Value> {
    let mut resource_attrs = BTreeMap::new();
    resource_attrs.insert("id".to_string(), Value::String(entity_id.to_string()));
    resource_attrs.insert(
        "status".to_string(),
        Value::String(defaults.initial_state.clone()),
    );
    if let Value::Object(fields) = params {
        for (key, value) in fields {
            resource_attrs.insert(key.clone(), value.clone());
        }
    }
    resource_attrs.insert("has_spec".to_string(), Value::Bool(defaults.has_spec));
    resource_attrs
}

pub(super) fn normalize_sub_write_params(sub_write: CompositeSubWrite) -> Value {
    let mut params = if sub_write.params.is_null() {
        Value::Object(Default::default())
    } else {
        sub_write.params
    };
    if let Some(obj) = params.as_object_mut() {
        obj.entry("Id".to_string())
            .or_insert(Value::String(sub_write.entity_id));
    }
    params
}

pub(super) fn composite_parent_idempotency(
    agent_ctx: &AgentContext,
    callback_params: &Value,
) -> String {
    if let Some(key) = agent_ctx.idempotency_key.as_deref() {
        return key.to_string();
    }

    let mut hasher = Sha256::new();
    hasher.update(b"composite-integration-result:");
    hasher.update(callback_params.to_string().as_bytes());
    format!("implicit:{:x}", hasher.finalize())
}

pub(super) fn build_composite_event(
    tenant: &TenantId,
    parent_entity_type: &str,
    parent_entity_id: &str,
    parent_action: &str,
    parent_idempotency: &str,
    prepared_sub_writes: &[PreparedCompositeSubWrite],
) -> CompositeEvent {
    CompositeEvent {
        tenant: tenant.as_str().to_string(),
        parent_entity_type: parent_entity_type.to_string(),
        parent_entity_id: parent_entity_id.to_string(),
        parent_action: parent_action.to_string(),
        composite_idempotency_key: parent_idempotency.to_string(),
        sub_writes: prepared_sub_writes
            .iter()
            .map(|write| CompositeEventSubWrite {
                index: write.idx,
                entity_type: write.entity_type.clone(),
                entity_id: write.entity_id.clone(),
                action: write.action.clone(),
                idempotency_key: write.idempotency_key.clone(),
            })
            .collect(),
    }
}

pub(super) fn composite_event_envelope(
    persistence_id: &str,
    event: &CompositeEvent,
) -> Result<PersistenceEnvelope, DispatchError> {
    let payload = serde_json::to_value(event)
        .map_err(|e| DispatchError::Internal(format!("failed to serialize CompositeEvent: {e}")))?;
    Ok(PersistenceEnvelope {
        sequence_nr: 0,
        event_type: COMPOSITE_EVENT_TYPE.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.to_string(),
        },
    })
}

pub(super) fn composite_envelope(
    persistence_id: &str,
    event: &crate::entity_actor::EntityEvent,
) -> Result<PersistenceEnvelope, DispatchError> {
    let payload = serde_json::to_value(event).map_err(|e| {
        DispatchError::Internal(format!("failed to serialize composite event: {e}"))
    })?;
    Ok(PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event.action.clone(),
        payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: event.timestamp,
            actor_id: persistence_id.to_string(),
        },
    })
}

pub(super) fn composite_batch_persistence_error(error: PersistenceError) -> DispatchError {
    match error {
        PersistenceError::ConcurrencyViolation { .. } => {
            DispatchError::Conflict(format!("composite batch persistence conflict: {error}"))
        }
        PersistenceError::Serialization(_) | PersistenceError::Storage(_) => {
            DispatchError::Internal(format!("composite batch persistence failed: {error}"))
        }
    }
}

pub(super) fn composite_storage_cap_error(error: CommonsStorageCapError) -> DispatchError {
    match error {
        CommonsStorageCapError::Exceeded(_)
        | CommonsStorageCapError::ReservationCapacityExhausted => {
            DispatchError::QuotaExceeded(error.to_string())
        }
        CommonsStorageCapError::OwnerSuspended(_) => DispatchError::AuthzDenied(error.to_string()),
        CommonsStorageCapError::MissingAttribution(_) | CommonsStorageCapError::Internal(_) => {
            DispatchError::Internal(error.to_string())
        }
    }
}

pub(super) fn composite_account_verification_error(
    error: CommonsAccountVerificationError,
) -> DispatchError {
    match error {
        CommonsAccountVerificationError::Required(_)
        | CommonsAccountVerificationError::MissingOwner(_)
        | CommonsAccountVerificationError::OwnerSuspended(_) => {
            DispatchError::AuthzDenied(error.to_string())
        }
        CommonsAccountVerificationError::Internal(_) => DispatchError::Internal(error.to_string()),
    }
}

pub(super) fn composite_app_uniqueness_error(error: CommonsAppUniquenessError) -> DispatchError {
    match error {
        CommonsAppUniquenessError::Conflict(_) => DispatchError::Conflict(error.to_string()),
        CommonsAppUniquenessError::Internal(_) => DispatchError::Internal(error.to_string()),
    }
}
