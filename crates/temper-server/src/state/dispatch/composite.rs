use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use temper_authz::SecurityContext;
use temper_jit::table::{CompositeActionMetadata, TransitionTable};
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, CompositeEvent, CompositeEventSubWrite, EventMetadata, PersistenceAppend,
    PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::EntityState;
use crate::entity_actor::effects::{
    FieldSyncMode, build_eval_context_with_xref, process_action_with_xref_and_field_mode,
};
use crate::request_context::AgentContext;
use crate::state::account_verification::CommonsAccountVerificationError;
use crate::state::app_uniqueness::CommonsAppUniquenessError;
use crate::state::storage_caps::{CommonsStorageCapError, CommonsStorageWrite};
use crate::storage::BackendLabel;

use super::DispatchError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompositeSubWrite {
    #[serde(alias = "target_entity", alias = "EntityType", alias = "entity")]
    pub entity_type: String,
    #[serde(alias = "entity_id", alias = "target_id", alias = "Id", alias = "id")]
    pub entity_id: String,
    pub action: String,
    #[serde(default = "empty_params")]
    pub params: Value,
}

#[derive(Debug, Clone)]
struct PreparedCompositeSubWrite {
    idx: usize,
    entity_type: String,
    entity_id: String,
    action: String,
    params: Value,
    idempotency_key: String,
}

#[derive(Debug)]
struct AtomicCompositeStream {
    entity_type: String,
    entity_id: String,
    state: EntityState,
    expected_sequence: u64,
    events: Vec<PersistenceEnvelope>,
}

fn empty_params() -> Value {
    Value::Object(Default::default())
}

impl crate::state::ServerState {
    pub(super) fn composite_metadata_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
    ) -> Result<Option<CompositeActionMetadata>, DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        Ok(table.composite_actions.get(action).cloned())
    }

    pub(super) fn reject_action_supplied_sub_writes(
        &self,
        entity_type: &str,
        action: &str,
        params: &Value,
    ) -> Result<(), DispatchError> {
        if has_sub_writes(params) {
            return Err(DispatchError::Internal(format!(
                "Composite action {entity_type}.{action} cannot accept caller-supplied sub_writes; sub-writes must be produced by a spec-declared integration result"
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn apply_composite_integration_result(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        callback_params: &Value,
        agent_ctx: &AgentContext,
    ) -> Result<bool, DispatchError> {
        if !has_sub_writes(callback_params) {
            return Ok(false);
        }

        let metadata = self
            .composite_metadata_for(tenant, entity_type, action)?
            .ok_or_else(|| {
                DispatchError::Internal(format!(
                    "Integration result for non-Composite action {entity_type}.{action} included sub_writes"
                ))
            })?;

        let sub_writes = parse_sub_writes(callback_params)?;
        validate_sub_writes(&metadata, &sub_writes)?;
        let parent_idempotency = composite_parent_idempotency(agent_ctx, callback_params);

        let _commons_guardrail_lock = self.acquire_commons_write_guardrail_lock(tenant).await;

        let composite_action_context = format!("composite:{entity_type}.{action}");
        let mut composite_agent_ctx = agent_ctx.clone();
        composite_agent_ctx.security_ctx = Some(
            agent_ctx
                .security_ctx
                .clone()
                .unwrap_or_else(|| SecurityContext::from_headers(&[]))
                .with_action_context(composite_action_context),
        );

        let prepared_sub_writes = self
            .prepare_composite_sub_writes(
                tenant,
                entity_type,
                entity_id,
                action,
                &sub_writes,
                &composite_agent_ctx,
                &parent_idempotency,
            )
            .await?;

        if self
            .apply_composite_sub_writes_atomic(
                tenant,
                entity_type,
                entity_id,
                action,
                &prepared_sub_writes,
                &parent_idempotency,
                &composite_agent_ctx,
            )
            .await?
        {
            return Ok(true);
        }

        for prepared in prepared_sub_writes {
            let mut sub_agent_ctx = composite_agent_ctx.clone();
            sub_agent_ctx.idempotency_key = Some(prepared.idempotency_key);

            let response = self
                .dispatch_tenant_action_core(
                    tenant,
                    &prepared.entity_type,
                    &prepared.entity_id,
                    &prepared.action,
                    prepared.params,
                    &sub_agent_ctx,
                    false,
                )
                .await?;

            if !response.success {
                return Err(DispatchError::Internal(response.error.unwrap_or_else(
                    || {
                        format!(
                            "composite {entity_type}.{action} sub-write {} failed",
                            prepared.idx
                        )
                    },
                )));
            }
        }

        Ok(true)
    }

    async fn apply_composite_sub_writes_atomic(
        &self,
        tenant: &TenantId,
        parent_entity_type: &str,
        parent_entity_id: &str,
        parent_action: &str,
        prepared_sub_writes: &[PreparedCompositeSubWrite],
        parent_idempotency: &str,
        _composite_agent_ctx: &AgentContext,
    ) -> Result<bool, DispatchError> {
        let Some((store, backend)) = self.event_journal() else {
            return Ok(false);
        };
        if prepared_sub_writes.is_empty() {
            return Ok(true);
        }

        let field_sync_mode = self.composite_batch_field_sync_mode(tenant, backend);
        let blob_store = self.blob_store_for_tenant(tenant).ok();
        let mut streams: BTreeMap<String, AtomicCompositeStream> = BTreeMap::new();
        let parent_persistence_id = format!("{tenant}:{parent_entity_type}:{parent_entity_id}");

        if !self
            .composite_event_already_persisted(&store, &parent_persistence_id, parent_idempotency)
            .await?
        {
            self.ensure_atomic_composite_stream(
                &mut streams,
                tenant,
                parent_entity_type,
                parent_entity_id,
            )
            .await?;
            let event = build_composite_event(
                tenant,
                parent_entity_type,
                parent_entity_id,
                parent_action,
                parent_idempotency,
                prepared_sub_writes,
            );
            let stream = streams
                .get_mut(&parent_persistence_id)
                .expect("parent stream inserted before composite event append");
            stream
                .events
                .push(composite_event_envelope(&parent_persistence_id, &event)?);
            stream.state.sequence_nr = stream.state.sequence_nr.saturating_add(1);
        }

        for write in prepared_sub_writes {
            let persistence_id = format!("{tenant}:{}:{}", write.entity_type, write.entity_id);
            self.ensure_atomic_composite_stream(
                &mut streams,
                tenant,
                &write.entity_type,
                &write.entity_id,
            )
            .await?;

            let table = self.transition_table_for_dispatch(tenant, &write.entity_type)?;
            let cross_entity_booleans = self
                .resolve_cross_entity_guards(
                    tenant,
                    &write.entity_type,
                    &write.entity_id,
                    &write.action,
                )
                .await;
            let stream = streams
                .get_mut(&persistence_id)
                .expect("stream inserted before processing sub-write");

            if stream
                .state
                .has_processed_idempotency_key(&write.idempotency_key)
            {
                continue;
            }

            let result = process_action_with_xref_and_field_mode(
                &mut stream.state,
                &table,
                &write.action,
                &write.params,
                &cross_entity_booleans,
                field_sync_mode,
            );
            if !result.success {
                return Err(DispatchError::Internal(result.error.unwrap_or_else(|| {
                    format!(
                        "composite {parent_entity_type}.{parent_action} sub-write {} failed during atomic staging",
                        write.idx
                    )
                })));
            }
            if !result.custom_effects.is_empty()
                || !result.scheduled_actions.is_empty()
                || !result.spawn_requests.is_empty()
            {
                return Ok(false);
            }
            if !result.overflow_blobs.is_empty() {
                let blob_store = blob_store.as_ref().ok_or_else(|| {
                    DispatchError::Internal(
                        "field-overflow blobs require a configured object blob store".to_string(),
                    )
                })?;
                crate::blobs::put_overflow_blobs(blob_store, &result.overflow_blobs)
                    .await
                    .map_err(|e| {
                        DispatchError::Internal(format!(
                            "field-overflow blob persistence failed during composite batch: {e}"
                        ))
                    })?;
            }

            let mut event = result
                .event
                .expect("successful process_action returns an event");
            event.idempotency_key = Some(write.idempotency_key.clone());
            stream
                .events
                .push(composite_envelope(&persistence_id, &event)?);
            stream.state.sequence_nr = stream.state.sequence_nr.saturating_add(1);
            stream.state.push_event_bounded(event);
        }

        let appends = streams
            .iter()
            .filter(|(_, stream)| !stream.events.is_empty())
            .map(|(persistence_id, stream)| PersistenceAppend {
                persistence_id: persistence_id.clone(),
                expected_sequence: stream.expected_sequence,
                events: stream.events.clone(),
            })
            .collect::<Vec<_>>();
        if appends.is_empty() {
            return Ok(true);
        }

        store
            .append_batch(&appends)
            .await
            .map_err(composite_batch_persistence_error)?;

        for stream in streams.values() {
            if stream.events.is_empty() {
                continue;
            }
            self.cache_entity_status(
                format!("{tenant}:{}:{}", stream.entity_type, stream.entity_id),
                stream.state.status.clone(),
            );
            self.clear_commons_storage_projection_cache_for_entity(&stream.entity_type);
            if let Some(query_plane) = self.query_plane_store() {
                let fields =
                    self.query_projection_fields(tenant, &stream.entity_type, &stream.state.fields);
                query_plane
                    .upsert_projection(
                        tenant.as_str(),
                        &stream.entity_type,
                        &stream.entity_id,
                        &stream.state.status,
                        &fields,
                        stream.state.sequence_nr,
                    )
                    .await
                    .map_err(|e| {
                        DispatchError::Internal(format!(
                            "query projection write failed after composite batch: {e}"
                        ))
                    })?;
            }
            self.stop_and_remove_entity(tenant, &stream.entity_type, &stream.entity_id);
            if !self
                .ensure_entity_loaded(tenant, &stream.entity_type, &stream.entity_id)
                .await
            {
                return Err(DispatchError::Internal(format!(
                    "composite batch committed {}:{} but failed to reload it",
                    stream.entity_type, stream.entity_id
                )));
            }
        }

        Ok(true)
    }

    async fn ensure_atomic_composite_stream(
        &self,
        streams: &mut BTreeMap<String, AtomicCompositeStream>,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), DispatchError> {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        if streams.contains_key(&persistence_id) {
            return Ok(());
        }

        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        let target_exists = self
            .ensure_entity_loaded(tenant, entity_type, entity_id)
            .await;
        let mut state = if target_exists {
            self.get_tenant_entity_state(tenant, entity_type, entity_id)
                .await
                .map_err(DispatchError::Internal)?
                .state
        } else {
            synthetic_initial_state(entity_type, entity_id, &table)
        };
        let expected_sequence = state.sequence_nr;
        let mut events = Vec::new();
        if !target_exists && expected_sequence == 0 && state.total_event_count == 0 {
            let bootstrap = crate::entity_actor::EntityEvent {
                action: "Created".to_string(),
                from_status: String::new(),
                to_status: state.status.clone(),
                timestamp: sim_now(),
                params: serde_json::json!({}),
                idempotency_key: None,
            };
            events.push(composite_envelope(&persistence_id, &bootstrap)?);
            state.sequence_nr = state.sequence_nr.saturating_add(1);
            state.push_event_bounded(bootstrap);
        }
        streams.insert(
            persistence_id,
            AtomicCompositeStream {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                state,
                expected_sequence,
                events,
            },
        );
        Ok(())
    }

    async fn composite_event_already_persisted(
        &self,
        store: &crate::storage::BoxedEventStore,
        parent_persistence_id: &str,
        parent_idempotency: &str,
    ) -> Result<bool, DispatchError> {
        let envelopes = store
            .read_events(parent_persistence_id, 0)
            .await
            .map_err(|e| {
                DispatchError::Internal(format!(
                    "failed to read parent journal before CompositeEvent append: {e}"
                ))
            })?;
        Ok(envelopes.iter().any(|env| {
            env.event_type == COMPOSITE_EVENT_TYPE
                && serde_json::from_value::<CompositeEvent>(env.payload.clone())
                    .is_ok_and(|event| event.composite_idempotency_key == parent_idempotency)
        }))
    }

    fn composite_batch_field_sync_mode(
        &self,
        tenant: &TenantId,
        backend: BackendLabel,
    ) -> FieldSyncMode {
        match backend {
            BackendLabel::Turso | BackendLabel::TursoRouted => FieldSyncMode::blob_refs_default(),
            _ if self.blob_store_for_tenant(tenant).is_ok() => FieldSyncMode::blob_refs_default(),
            _ => FieldSyncMode::InlineTruncate,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_composite_sub_writes(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        sub_writes: &[CompositeSubWrite],
        composite_agent_ctx: &AgentContext,
        parent_idempotency: &str,
    ) -> Result<Vec<PreparedCompositeSubWrite>, DispatchError> {
        let sub_security_ctx = composite_agent_ctx.security_ctx.as_ref().ok_or_else(|| {
            DispatchError::Internal(
                "composite sub-write authorization requires a security context".to_string(),
            )
        })?;
        let mut prepared = Vec::with_capacity(sub_writes.len());

        for (idx, sub_write) in sub_writes.iter().cloned().enumerate() {
            let sub_entity_type = sub_write.entity_type.clone();
            let sub_entity_id = sub_write.entity_id.clone();
            let sub_action = sub_write.action.clone();
            let sub_params = normalize_sub_write_params(sub_write);

            if !self
                .is_entity_type_governed(tenant, &sub_entity_type)
                .map_err(DispatchError::Internal)?
            {
                return Err(DispatchError::Ungoverned(sub_entity_type));
            }

            let resource_attrs = self
                .composite_sub_write_auth_resource_attrs(
                    tenant,
                    &sub_entity_type,
                    &sub_entity_id,
                    &sub_action,
                    &sub_params,
                )
                .await?;

            self.authorize_with_context(
                sub_security_ctx,
                &sub_action,
                &sub_entity_type,
                &resource_attrs,
                tenant.as_str(),
            )
            .map_err(|denial| {
                DispatchError::AuthzDenied(format!(
                    "composite {entity_type}.{action} sub-write {idx} denied for {sub_entity_type}.{sub_action}: {denial}"
                ))
            })?;

            prepared.push(PreparedCompositeSubWrite {
                idx,
                entity_type: sub_entity_type,
                entity_id: sub_entity_id,
                action: sub_action,
                params: sub_params,
                idempotency_key: format!(
                    "composite:{tenant}:{entity_type}:{entity_id}:{action}:{parent_idempotency}:subwrite:{idx}"
                ),
            });
        }

        for write in &prepared {
            self.preflight_composite_sub_write_transition(tenant, entity_type, action, write)
                .await?;
        }

        let storage_writes = prepared
            .iter()
            .map(|write| CommonsStorageWrite {
                entity_type: write.entity_type.clone(),
                entity_id: write.entity_id.clone(),
                action: write.action.clone(),
                fields: write.params.clone(),
            })
            .collect::<Vec<_>>();
        for write in &storage_writes {
            self.enforce_commons_verified_owner_for_write(
                tenant,
                &write.entity_type,
                &write.fields,
            )
            .await
            .map_err(composite_account_verification_error)?;
            self.enforce_commons_app_name_unique_for_write(
                tenant,
                &write.entity_type,
                &write.entity_id,
                &write.fields,
            )
            .await
            .map_err(composite_app_uniqueness_error)?;
        }
        self.enforce_commons_storage_caps_for_writes(tenant, &storage_writes)
            .await
            .map_err(composite_storage_cap_error)?;

        Ok(prepared)
    }

    async fn preflight_composite_sub_write_transition(
        &self,
        tenant: &TenantId,
        parent_entity_type: &str,
        parent_action: &str,
        write: &PreparedCompositeSubWrite,
    ) -> Result<(), DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, &write.entity_type)?;
        let target_exists = self
            .ensure_entity_loaded(tenant, &write.entity_type, &write.entity_id)
            .await;
        let target_state = if target_exists {
            self.get_tenant_entity_state(tenant, &write.entity_type, &write.entity_id)
                .await
                .map_err(DispatchError::Internal)?
                .state
        } else {
            synthetic_initial_state(&write.entity_type, &write.entity_id, &table)
        };

        if target_state.has_processed_idempotency_key(&write.idempotency_key) {
            return Ok(());
        }

        if !target_state.can_accept_event() {
            return Err(DispatchError::Internal(format!(
                "composite {parent_entity_type}.{parent_action} sub-write {} would exceed the event budget for {}:{}",
                write.idx, write.entity_type, write.entity_id
            )));
        }

        let cross_entity_booleans = self
            .resolve_cross_entity_guards(
                tenant,
                &write.entity_type,
                &write.entity_id,
                &write.action,
            )
            .await;
        let eval_ctx = build_eval_context_with_xref(&target_state, &cross_entity_booleans);
        match table.evaluate_ctx(&target_state.status, &eval_ctx, &write.action) {
            Some(result) if result.success => Ok(()),
            Some(_) => Err(DispatchError::Conflict(format!(
                "composite {parent_entity_type}.{parent_action} sub-write {} would fail: action '{}' is not valid from state '{}'",
                write.idx, write.action, target_state.status
            ))),
            None => Err(DispatchError::Internal(format!(
                "composite {parent_entity_type}.{parent_action} sub-write {} would fail: unknown action '{}'",
                write.idx, write.action
            ))),
        }
    }

    async fn composite_sub_write_auth_resource_attrs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: &Value,
    ) -> Result<BTreeMap<String, Value>, DispatchError> {
        if action == "Create" {
            return self.composite_create_resource_attrs(tenant, entity_type, entity_id, params);
        }

        if !self
            .ensure_entity_loaded(tenant, entity_type, entity_id)
            .await
        {
            return Err(DispatchError::Internal(format!(
                "composite sub-write target {entity_type}:{entity_id} does not exist"
            )));
        }

        self.load_authz_resource_snapshot(tenant, entity_type, entity_id)
            .await
            .map(|snapshot| snapshot.resource_attrs)
            .map_err(DispatchError::Internal)
    }

    fn composite_create_resource_attrs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        params: &Value,
    ) -> Result<BTreeMap<String, Value>, DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        let mut resource_attrs = BTreeMap::new();
        resource_attrs.insert("id".to_string(), Value::String(entity_id.to_string()));
        resource_attrs.insert(
            "status".to_string(),
            Value::String(table.initial_state.clone()),
        );
        if let Value::Object(fields) = params {
            for (key, value) in fields {
                resource_attrs.insert(key.clone(), value.clone());
            }
        }
        let has_spec = self
            .has_registered_spec(tenant, entity_type)
            .map_err(DispatchError::Internal)?;
        resource_attrs.insert("has_spec".to_string(), Value::Bool(has_spec));
        Ok(resource_attrs)
    }

    fn transition_table_for_dispatch(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<Arc<TransitionTable>, DispatchError> {
        if let Some(table) = self
            .registry
            .read()
            .map_err(|e| DispatchError::Internal(format!("registry lock poisoned: {e}")))?
            .get_table(tenant, entity_type)
        {
            return Ok(table);
        }

        self.transition_tables
            .get(entity_type)
            .cloned()
            .ok_or_else(|| DispatchError::Ungoverned(entity_type.to_string()))
    }

    #[allow(dead_code)]
    async fn ensure_composite_entry_transition_allowed(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
    ) -> Result<(), DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        let current = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
            .map_err(DispatchError::Internal)?;
        let cross_entity_booleans = self
            .resolve_cross_entity_guards(tenant, entity_type, entity_id, action)
            .await;
        let eval_ctx = build_eval_context_with_xref(&current.state, &cross_entity_booleans);

        match table.evaluate_ctx(&current.state.status, &eval_ctx, action) {
            Some(result) if result.success => Ok(()),
            Some(_) => Err(DispatchError::Internal(format!(
                "Composite action '{action}' not valid from state '{}'",
                current.state.status
            ))),
            None => Err(DispatchError::Internal(format!(
                "Unknown composite action: {action}"
            ))),
        }
    }
}

fn synthetic_initial_state(
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
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    }
}

fn has_sub_writes(params: &Value) -> bool {
    params.get("sub_writes").is_some() || params.get("SubWrites").is_some()
}

fn parse_sub_writes(params: &Value) -> Result<Vec<CompositeSubWrite>, DispatchError> {
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

fn validate_sub_writes(
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

fn normalize_sub_write_params(sub_write: CompositeSubWrite) -> Value {
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

fn composite_parent_idempotency(agent_ctx: &AgentContext, callback_params: &Value) -> String {
    if let Some(key) = agent_ctx.idempotency_key.as_deref() {
        return key.to_string();
    }

    let mut hasher = Sha256::new();
    hasher.update(b"composite-integration-result:");
    hasher.update(callback_params.to_string().as_bytes());
    format!("implicit:{:x}", hasher.finalize())
}

fn build_composite_event(
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

fn composite_event_envelope(
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

fn composite_envelope(
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

fn composite_batch_persistence_error(error: PersistenceError) -> DispatchError {
    match error {
        PersistenceError::ConcurrencyViolation { .. } => {
            DispatchError::Conflict(format!("composite batch persistence conflict: {error}"))
        }
        PersistenceError::Serialization(_) | PersistenceError::Storage(_) => {
            DispatchError::Internal(format!("composite batch persistence failed: {error}"))
        }
    }
}

fn composite_storage_cap_error(error: CommonsStorageCapError) -> DispatchError {
    match error {
        CommonsStorageCapError::Exceeded(_) => DispatchError::QuotaExceeded(error.to_string()),
        CommonsStorageCapError::OwnerSuspended(_) => DispatchError::AuthzDenied(error.to_string()),
        CommonsStorageCapError::MissingAttribution(_) | CommonsStorageCapError::Internal(_) => {
            DispatchError::Internal(error.to_string())
        }
    }
}

fn composite_account_verification_error(error: CommonsAccountVerificationError) -> DispatchError {
    match error {
        CommonsAccountVerificationError::Required(_)
        | CommonsAccountVerificationError::MissingOwner(_)
        | CommonsAccountVerificationError::OwnerSuspended(_) => {
            DispatchError::AuthzDenied(error.to_string())
        }
        CommonsAccountVerificationError::Internal(_) => DispatchError::Internal(error.to_string()),
    }
}

fn composite_app_uniqueness_error(error: CommonsAppUniquenessError) -> DispatchError {
    match error {
        CommonsAppUniquenessError::Conflict(_) => DispatchError::Conflict(error.to_string()),
        CommonsAppUniquenessError::Internal(_) => DispatchError::Internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;
    #[cfg(feature = "sim")]
    use temper_store_sim::SimEventStore;

    use crate::request_context::AgentContext;
    use crate::state::ServerState;
    #[cfg(feature = "sim")]
    use crate::storage::StorageStack;

    use super::*;

    #[test]
    fn implicit_composite_idempotency_changes_with_integration_result() {
        let agent = AgentContext::for_service("composite-test");
        let first = composite_parent_idempotency(
            &agent,
            &json!({
                "sub_writes": [{
                    "entity_type": "Ref",
                    "entity_id": "rf-1",
                    "action": "Create",
                    "params": {"Name": "refs/heads/topic"}
                }]
            }),
        );
        let second = composite_parent_idempotency(
            &agent,
            &json!({
                "sub_writes": [{
                    "entity_type": "Ref",
                    "entity_id": "rf-1",
                    "action": "Delete",
                    "params": {}
                }]
            }),
        );

        assert_ne!(first, second);
    }

    const COMPOSITE_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CompositeTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Parent">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Child">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="App">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="OwnerId" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Parents" EntityType="Temper.CompositeTest.Parent"/>
        <EntitySet Name="Children" EntityType="Temper.CompositeTest.Child"/>
        <EntitySet Name="Apps" EntityType="Temper.CompositeTest.App"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

    const PARENT_IOA: &str = r#"
[automaton]
name = "Parent"
states = ["Active"]
initial = "Active"

[[action]]
name = "CreateChild"
kind = "Composite"
from = ["Active"]
to = "Active"
params = ["Reason"]

[[action.sub_writes]]
target_entity = "Child"
action = "Create"
generated_from = "child"

[[action.sub_writes]]
target_entity = "App"
action = "Create"
generated_from = "app_metadata"
"#;

    const CHILD_IOA: &str = r#"
[automaton]
name = "Child"
states = ["Draft", "Active"]
initial = "Draft"

[[action]]
name = "Create"
kind = "input"
from = ["Draft"]
to = "Active"
params = ["Name"]
"#;

    const APP_IOA: &str = r#"
[automaton]
name = "App"
states = ["Active"]
initial = "Active"

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
to = "Active"
params = ["OwnerId", "Name"]
"#;

    fn composite_test_state() -> ServerState {
        let csdl = parse_csdl(COMPOSITE_CSDL).expect("test CSDL should parse");
        let mut specs = BTreeMap::new();
        specs.insert("Parent".to_string(), PARENT_IOA.to_string());
        specs.insert("Child".to_string(), CHILD_IOA.to_string());
        specs.insert("App".to_string(), APP_IOA.to_string());
        ServerState::with_specs(
            ActorSystem::new("composite-dispatch-test"),
            csdl,
            COMPOSITE_CSDL.to_string(),
            specs,
        )
        .expect("test state should build")
    }

    #[cfg(feature = "sim")]
    fn composite_test_state_with_store(store: SimEventStore) -> ServerState {
        let csdl = parse_csdl(COMPOSITE_CSDL).expect("test CSDL should parse");
        let mut specs = BTreeMap::new();
        specs.insert("Parent".to_string(), PARENT_IOA.to_string());
        specs.insert("Child".to_string(), CHILD_IOA.to_string());
        specs.insert("App".to_string(), APP_IOA.to_string());
        ServerState::with_storage_stack(
            ActorSystem::new("composite-dispatch-test"),
            csdl,
            COMPOSITE_CSDL.to_string(),
            specs,
            StorageStack::from_sim(store, None),
        )
        .expect("test state should build")
    }

    #[tokio::test]
    async fn composite_action_rejects_caller_supplied_sub_writes() {
        let state = composite_test_state();
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");

        let err = state
            .dispatch_tenant_action(
                &tenant,
                "Parent",
                "parent-1",
                "CreateChild",
                json!({
                    "Reason": "unit-test",
                    "sub_writes": [{
                        "entity_type": "Child",
                        "entity_id": "child-1",
                        "action": "Create",
                        "params": { "Name": "created through composite" }
                    }]
                }),
                &agent,
            )
            .await
            .expect_err("caller-supplied sub_writes should be rejected");

        assert!(
            err.contains("cannot accept caller-supplied sub_writes"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn composite_integration_result_executes_declared_sub_writes() {
        let state = composite_test_state();
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");

        let response = state
            .dispatch_tenant_action(
                &tenant,
                "Parent",
                "parent-1",
                "CreateChild",
                json!({ "Reason": "unit-test" }),
                &agent,
            )
            .await
            .expect("composite parent action should run");

        assert!(response.success);
        assert_eq!(response.state.status, "Active");
        assert!(response.state.fields.get("sub_writes").is_none());

        let applied = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-1",
                "CreateChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "Child",
                        "entity_id": "child-1",
                        "action": "Create",
                        "params": { "Name": "created through composite integration" }
                    }]
                }),
                &agent,
            )
            .await
            .expect("composite integration result should apply");

        assert!(applied);

        let child = state
            .get_tenant_entity_state(&tenant, "Child", "child-1")
            .await
            .expect("child state should be readable");
        assert_eq!(child.state.status, "Active");
        assert_eq!(
            child.state.fields.get("Name"),
            Some(&json!("created through composite integration"))
        );
    }

    #[tokio::test]
    async fn composite_sub_write_authorization_receives_action_context() {
        let state = composite_test_state();
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");

        state
            .authz
            .reload_policies(
                r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Parent.CreateChild"
                };
                "#,
            )
            .expect("policy should load");

        let applied = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-auth",
                "CreateChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "Child",
                        "entity_id": "child-auth-ok",
                        "action": "Create",
                        "params": { "Name": "authorized through action_context" }
                    }]
                }),
                &agent,
            )
            .await
            .expect("composite sub-write should be authorized by action_context");
        assert!(applied);

        state
            .authz
            .reload_policies(
                r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Other.Action"
                };
                "#,
            )
            .expect("policy should load");

        let err = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-auth",
                "CreateChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "Child",
                        "entity_id": "child-auth-denied",
                        "action": "Create",
                        "params": { "Name": "should be denied" }
                    }]
                }),
                &agent,
            )
            .await
            .expect_err("mismatched action_context should deny sub-write")
            .to_string();
        assert!(
            err.contains("sub-write 0 denied"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn composite_app_create_sub_write_authorization_can_enforce_owner_scope() {
        let state = composite_test_state();
        let tenant = TenantId::default();
        let mut agent = AgentContext::default();
        agent.security_ctx = Some(SecurityContext::from_headers(&[
            ("X-Temper-Principal-Id".to_string(), "alice".to_string()),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
        ]));

        state
            .authz
            .reload_policies(
                r#"
                permit(
                  principal,
                  action == Action::"Create",
                  resource is App
                );

                forbid(
                  principal,
                  action == Action::"Create",
                  resource is App
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Parent.CreateChild" &&
                  !(resource.OwnerId == principal.accountId ||
                    (principal has scopes &&
                     principal.scopes.contains("admin:repos")))
                };
                "#,
            )
            .expect("policy should load");

        let err = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-owner-scope",
                "CreateChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "App",
                        "entity_id": "app-bob-owned",
                        "action": "Create",
                        "params": { "OwnerId": "bob", "Name": "bob-app" }
                    }]
                }),
                &agent,
            )
            .await
            .expect_err("caller must not create a composite App row under another owner")
            .to_string();
        assert!(
            err.contains("sub-write 0 denied"),
            "unexpected error: {err}"
        );
        assert!(!state.entity_exists(&tenant, "App", "app-bob-owned"));

        let allowed = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-owner-scope",
                "CreateChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "App",
                        "entity_id": "app-alice-owned",
                        "action": "Create",
                        "params": { "OwnerId": "alice", "Name": "alice-app" }
                    }]
                }),
                &agent,
            )
            .await
            .expect("caller should create a composite App row under their own owner");
        assert!(allowed);
        assert!(state.entity_exists(&tenant, "App", "app-alice-owned"));
    }

    #[cfg(feature = "sim")]
    #[tokio::test]
    async fn composite_preflights_sub_write_auth_before_persisting_any_write() {
        let store = SimEventStore::no_faults(40);
        let state = composite_test_state_with_store(store.clone());
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");

        state
            .authz
            .reload_policies(
                r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Parent.CreateChild" &&
                  resource.id == "child-preflight-first"
                };
                "#,
            )
            .expect("policy should load");

        let err = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-preflight",
                "CreateChild",
                &json!({
                    "sub_writes": [
                        {
                            "entity_type": "Child",
                            "entity_id": "child-preflight-first",
                            "action": "Create",
                            "params": { "Name": "would be allowed" }
                        },
                        {
                            "entity_type": "Child",
                            "entity_id": "child-preflight-denied",
                            "action": "Create",
                            "params": { "Name": "should be denied" }
                        }
                    ]
                }),
                &agent,
            )
            .await
            .expect_err("second sub-write should be denied during preflight")
            .to_string();

        assert!(
            err.contains("sub-write 1 denied"),
            "unexpected error: {err}"
        );
        assert!(
            store
                .dump_journal("default:Child:child-preflight-first")
                .is_empty(),
            "authorized earlier sub-write should not be persisted before later preflight denial"
        );
        assert!(
            store
                .dump_journal("default:Child:child-preflight-denied")
                .is_empty(),
            "denied sub-write should not be persisted"
        );
        assert!(!state.entity_exists(&tenant, "Child", "child-preflight-first"));
        assert!(!state.entity_exists(&tenant, "Child", "child-preflight-denied"));
    }

    #[cfg(feature = "sim")]
    #[tokio::test]
    async fn composite_preflights_sub_write_transition_before_persisting_any_write() {
        let store = SimEventStore::no_faults(41);
        let state = composite_test_state_with_store(store.clone());
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");

        let existing = state
            .dispatch_tenant_action(
                &tenant,
                "Child",
                "child-transition-existing",
                "Create",
                json!({ "Name": "already active" }),
                &agent,
            )
            .await
            .expect("existing child create should run");
        assert!(existing.success);

        let err = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-transition-preflight",
                "CreateChild",
                &json!({
                    "sub_writes": [
                        {
                            "entity_type": "Child",
                            "entity_id": "child-transition-first",
                            "action": "Create",
                            "params": { "Name": "would otherwise persist first" }
                        },
                        {
                            "entity_type": "Child",
                            "entity_id": "child-transition-existing",
                            "action": "Create",
                            "params": { "Name": "invalid from Active" }
                        }
                    ]
                }),
                &agent,
            )
            .await
            .expect_err("second sub-write should fail transition preflight")
            .to_string();

        assert!(
            err.contains("sub-write 1 would fail"),
            "unexpected error: {err}"
        );
        assert!(
            store
                .dump_journal("default:Child:child-transition-first")
                .is_empty(),
            "earlier sub-write should not persist before later transition preflight failure"
        );
        assert!(
            !state.entity_exists(&tenant, "Child", "child-transition-first"),
            "earlier sub-write actor should not be spawned"
        );
        assert_eq!(
            store
                .dump_journal("default:Child:child-transition-existing")
                .len(),
            2,
            "existing target should keep only its bootstrap and original Create events"
        );
    }

    #[cfg(feature = "sim")]
    #[tokio::test]
    async fn composite_atomic_batch_conflict_leaves_all_sub_write_journals_empty() {
        let store = SimEventStore::no_faults(42);
        let state = composite_test_state_with_store(store.clone());
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");

        store.inject_concurrency_violations("default:Child:child-atomic-second", 1);

        let err = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-atomic-batch",
                "CreateChild",
                &json!({
                    "sub_writes": [
                        {
                            "entity_type": "Child",
                            "entity_id": "child-atomic-first",
                            "action": "Create",
                            "params": { "Name": "must not persist" }
                        },
                        {
                            "entity_type": "Child",
                            "entity_id": "child-atomic-second",
                            "action": "Create",
                            "params": { "Name": "injected conflict" }
                        }
                    ]
                }),
                &agent,
            )
            .await
            .expect_err("atomic batch conflict should reject the whole composite")
            .to_string();

        assert!(
            err.contains("composite batch persistence conflict"),
            "unexpected error: {err}"
        );
        assert!(
            store
                .dump_journal("default:Child:child-atomic-first")
                .is_empty(),
            "first sub-write journal must stay empty when a later stream conflicts"
        );
        assert!(
            store
                .dump_journal("default:Child:child-atomic-second")
                .is_empty(),
            "conflicting sub-write journal must also stay empty"
        );
        assert!(!state.entity_exists(&tenant, "Child", "child-atomic-first"));
        assert!(!state.entity_exists(&tenant, "Child", "child-atomic-second"));
    }

    #[cfg(feature = "sim")]
    #[tokio::test]
    async fn composite_atomic_batch_records_parent_composite_event_once() {
        let store = SimEventStore::no_faults(40);
        let state = composite_test_state_with_store(store.clone());
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");
        let callback_params = json!({
            "sub_writes": [{
                "entity_type": "Child",
                "entity_id": "child-composite-event",
                "action": "Create",
                "params": { "Name": "recorded through CompositeEvent" }
            }]
        });

        state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-composite-event",
                "CreateChild",
                &callback_params,
                &agent,
            )
            .await
            .expect("composite result should apply");

        let parent_pid = "default:Parent:parent-composite-event";
        let parent_journal = store.dump_journal(parent_pid);
        assert_eq!(
            parent_journal
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec!["Created", COMPOSITE_EVENT_TYPE]
        );
        let composite_event =
            serde_json::from_value::<CompositeEvent>(parent_journal[1].payload.clone())
                .expect("CompositeEvent payload should decode");
        assert_eq!(composite_event.parent_entity_type, "Parent");
        assert_eq!(composite_event.parent_entity_id, "parent-composite-event");
        assert_eq!(composite_event.parent_action, "CreateChild");
        assert_eq!(composite_event.sub_writes.len(), 1);
        assert_eq!(composite_event.sub_writes[0].entity_type, "Child");
        assert_eq!(
            composite_event.sub_writes[0].entity_id,
            "child-composite-event"
        );
        assert_eq!(composite_event.sub_writes[0].action, "Create");
        assert!(
            composite_event.sub_writes[0]
                .idempotency_key
                .contains("subwrite:0")
        );

        let restarted = composite_test_state_with_store(store.clone());
        let parent = restarted
            .get_tenant_entity_state(&tenant, "Parent", "parent-composite-event")
            .await
            .expect("parent should hydrate from journal");
        assert_eq!(parent.state.status, "Active");
        assert_eq!(parent.state.sequence_nr, 2);
        assert!(parent.state.fields.get("sub_writes").is_none());

        restarted
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-composite-event",
                "CreateChild",
                &callback_params,
                &agent,
            )
            .await
            .expect("duplicate composite result should be idempotent");
        assert_eq!(
            store.dump_journal(parent_pid).len(),
            parent_journal.len(),
            "duplicate composite callback must not append a second CompositeEvent"
        );
    }

    #[cfg(feature = "sim")]
    #[tokio::test]
    async fn composite_sub_write_idempotency_survives_actor_restart() {
        let store = SimEventStore::no_faults(40);
        let state = composite_test_state_with_store(store.clone());
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");
        let callback_params = json!({
            "sub_writes": [{
                "entity_type": "Child",
                "entity_id": "child-replay",
                "action": "Create",
                "params": { "Name": "created once" }
            }]
        });

        let applied = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-1",
                "CreateChild",
                &callback_params,
                &agent,
            )
            .await
            .expect("first composite result should apply");
        assert!(applied);

        let child_pid = "default:Child:child-replay";
        let first_journal_len = store.dump_journal(child_pid).len();
        assert!(
            first_journal_len >= 2,
            "child journal should contain bootstrap + Create event"
        );

        let restarted = composite_test_state_with_store(store.clone());
        let replayed = restarted
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-1",
                "CreateChild",
                &callback_params,
                &agent,
            )
            .await
            .expect("duplicate composite result should be idempotent after replay");
        assert!(replayed);

        let child = restarted
            .get_tenant_entity_state(&tenant, "Child", "child-replay")
            .await
            .expect("child should still be readable");
        assert_eq!(child.state.status, "Active");
        assert_eq!(child.state.fields.get("Name"), Some(&json!("created once")));
        assert_eq!(
            store.dump_journal(child_pid).len(),
            first_journal_len,
            "duplicate sub-write should not append a second Create event"
        );
    }

    #[cfg(feature = "sim")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn composite_atomic_batch_handles_concurrent_multi_entity_results() {
        const COMPOSITES: usize = 12;
        const CHILDREN_PER_COMPOSITE: usize = 3;

        let store = SimEventStore::no_faults(44);
        let state = composite_test_state_with_store(store.clone());
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");

        let mut handles = Vec::new();
        for composite_idx in 0..COMPOSITES {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent = agent.clone();
            handles.push(tokio::spawn(async move {
                let parent_id = format!("parent-stress-{composite_idx}");
                let mut sub_writes = Vec::new();
                for child_idx in 0..CHILDREN_PER_COMPOSITE {
                    sub_writes.push(json!({
                        "entity_type": "Child",
                        "entity_id": format!("child-stress-{composite_idx}-{child_idx}"),
                        "action": "Create",
                        "params": {
                            "Name": format!("child {composite_idx}/{child_idx}")
                        }
                    }));
                }
                sub_writes.push(json!({
                    "entity_type": "App",
                    "entity_id": format!("app-stress-{composite_idx}"),
                    "action": "Create",
                    "params": {
                        "OwnerId": format!("owner-{composite_idx}"),
                        "Name": format!("app-{composite_idx}")
                    }
                }));

                let applied = state
                    .apply_composite_integration_result(
                        &tenant,
                        "Parent",
                        &parent_id,
                        "CreateChild",
                        &json!({ "sub_writes": sub_writes }),
                        &agent,
                    )
                    .await
                    .map_err(|err| err.to_string())?;
                Ok::<_, String>((parent_id, applied))
            }));
        }

        let mut parent_ids = Vec::new();
        for handle in handles {
            let (parent_id, applied) = handle
                .await
                .expect("concurrent composite task should join")
                .expect("concurrent composite result should apply");
            assert!(applied);
            parent_ids.push(parent_id);
        }

        for parent_id in parent_ids {
            let composite_idx = parent_id
                .strip_prefix("parent-stress-")
                .expect("stress parent id should include numeric suffix")
                .parse::<usize>()
                .expect("stress parent suffix should parse");
            let parent_journal = store.dump_journal(&format!("default:Parent:{parent_id}"));
            assert_eq!(
                parent_journal
                    .iter()
                    .map(|event| event.event_type.as_str())
                    .collect::<Vec<_>>(),
                vec!["Created", COMPOSITE_EVENT_TYPE],
                "parent {parent_id} should record one replay-safe CompositeEvent"
            );
            let composite_event =
                serde_json::from_value::<CompositeEvent>(parent_journal[1].payload.clone())
                    .expect("CompositeEvent payload should decode");
            assert_eq!(composite_event.sub_writes.len(), CHILDREN_PER_COMPOSITE + 1);

            for child_idx in 0..CHILDREN_PER_COMPOSITE {
                let child_id = format!("child-stress-{composite_idx}-{child_idx}");
                let child = state
                    .get_tenant_entity_state(&tenant, "Child", &child_id)
                    .await
                    .expect("stress child should be readable");
                assert_eq!(child.state.status, "Active");
                assert_eq!(
                    child.state.fields.get("Name"),
                    Some(&json!(format!("child {composite_idx}/{child_idx}")))
                );
            }

            let app_id = format!("app-stress-{composite_idx}");
            let app = state
                .get_tenant_entity_state(&tenant, "App", &app_id)
                .await
                .expect("stress app should be readable");
            assert_eq!(
                app.state.fields.get("OwnerId"),
                Some(&json!(format!("owner-{composite_idx}")))
            );
            assert_eq!(
                app.state.fields.get("Name"),
                Some(&json!(format!("app-{composite_idx}")))
            );
        }
    }

    #[tokio::test]
    async fn commons_composite_rejects_duplicate_owner_app_name_before_dispatch() {
        let state = composite_test_state();
        state.enable_commons_guardrails("default");
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");

        let first = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-app-name",
                "CreateChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "App",
                        "entity_id": "app-alice-notes",
                        "action": "Create",
                        "params": { "OwnerId": "alice", "Name": "notes" }
                    }]
                }),
                &agent,
            )
            .await
            .expect("first owner/app name should apply");
        assert!(first);

        let err = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-app-name",
                "CreateChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "App",
                        "entity_id": "app-alice-notes-copy",
                        "action": "Create",
                        "params": { "OwnerId": "Alice", "Name": "Notes" }
                    }]
                }),
                &agent,
            )
            .await
            .expect_err("duplicate owner/app name should be rejected")
            .to_string();

        assert!(
            err.contains("alice/Notes") || err.contains("Alice/Notes"),
            "unexpected error: {err}"
        );
        assert!(!state.entity_exists(&tenant, "App", "app-alice-notes-copy"));
    }

    #[cfg(feature = "sim")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn commons_composite_app_name_uniqueness_serializes_concurrent_creates() {
        let store = SimEventStore::no_faults(43);
        let state = composite_test_state_with_store(store.clone());
        state.enable_commons_guardrails("default");
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");
        let attempts = [
            ("parent-app-race-a", "app-race-a"),
            ("parent-app-race-b", "app-race-b"),
        ];

        let mut handles = Vec::new();
        for (parent_id, app_id) in attempts {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent = agent.clone();
            handles.push(tokio::spawn(async move {
                let result = state
                    .apply_composite_integration_result(
                        &tenant,
                        "Parent",
                        parent_id,
                        "CreateChild",
                        &json!({
                            "sub_writes": [{
                                "entity_type": "App",
                                "entity_id": app_id,
                                "action": "Create",
                                "params": { "OwnerId": "alice", "Name": "Notes" }
                            }]
                        }),
                        &agent,
                    )
                    .await
                    .map_err(|err| err.to_string());
                (parent_id.to_string(), app_id.to_string(), result)
            }));
        }

        let mut outcomes = Vec::new();
        for handle in handles {
            outcomes.push(handle.await.expect("concurrent task should finish"));
        }

        let successes = outcomes
            .iter()
            .filter(|(_, _, result)| matches!(result, Ok(true)))
            .count();
        let conflicts = outcomes
            .iter()
            .filter(
                |(_, _, result)| matches!(result, Err(err) if err.contains("already registered")),
            )
            .count();
        assert_eq!(
            successes, 1,
            "exactly one concurrent composite should create alice/Notes: {outcomes:?}"
        );
        assert_eq!(
            conflicts, 1,
            "the racing composite should fail closed with an app-name conflict: {outcomes:?}"
        );

        let persisted_apps = outcomes
            .iter()
            .filter(|(_, app_id, _)| state.entity_exists(&tenant, "App", app_id))
            .collect::<Vec<_>>();
        assert_eq!(
            persisted_apps.len(),
            1,
            "only the winning App row should exist after the race"
        );

        for (parent_id, app_id, result) in outcomes {
            let parent_journal = store.dump_journal(&format!("default:Parent:{parent_id}"));
            match result {
                Ok(true) => {
                    assert_eq!(
                        parent_journal
                            .iter()
                            .map(|event| event.event_type.as_str())
                            .collect::<Vec<_>>(),
                        vec!["Created", COMPOSITE_EVENT_TYPE],
                        "winning parent should record exactly one CompositeEvent"
                    );
                    let app = state
                        .get_tenant_entity_state(&tenant, "App", &app_id)
                        .await
                        .expect("winning app should be readable");
                    assert_eq!(app.state.fields.get("OwnerId"), Some(&json!("alice")));
                    assert_eq!(app.state.fields.get("Name"), Some(&json!("Notes")));
                }
                Err(err) => {
                    assert!(
                        err.contains("already registered"),
                        "unexpected losing result: {err}"
                    );
                    assert!(
                        parent_journal.is_empty(),
                        "losing parent journal must remain empty when uniqueness preflight rejects it"
                    );
                    assert!(
                        !state.entity_exists(&tenant, "App", &app_id),
                        "losing App row must not be persisted"
                    );
                }
                Ok(false) => panic!("composite should not fall back for simple App.Create"),
            }
        }
    }

    #[tokio::test]
    async fn composite_integration_result_rejects_undeclared_sub_write() {
        let state = composite_test_state();
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("composite-test");

        let err = state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-1",
                "CreateChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "Parent",
                        "entity_id": "parent-2",
                        "action": "CreateChild",
                        "params": {}
                    }]
                }),
                &agent,
            )
            .await
            .expect_err("undeclared sub-write should be rejected");

        let err = err.to_string();
        assert!(err.contains("is not declared"), "unexpected error: {err}");
    }
}
