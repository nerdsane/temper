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

mod helpers;
mod projection;
use helpers::*;

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
    preflight_target: Option<PreflightCompositeTarget>,
    uses_parent_gate: bool,
}

struct AtomicCompositeParent<'a> {
    tenant: &'a TenantId,
    entity_type: &'a str,
    entity_id: &'a str,
    action: &'a str,
    idempotency: &'a str,
    record_event: bool,
}

#[derive(Debug, Clone)]
struct PreflightCompositeTarget {
    target_existed: bool,
    state: EntityState,
}

#[derive(Debug)]
struct AtomicCompositeStream {
    entity_type: String,
    entity_id: String,
    target_existed: bool,
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
                &metadata,
                &composite_agent_ctx,
                &parent_idempotency,
            )
            .await?;

        if self
            .apply_composite_sub_writes_atomic(
                AtomicCompositeParent {
                    tenant,
                    entity_type,
                    entity_id,
                    action,
                    idempotency: &parent_idempotency,
                    record_event: metadata.record_parent_event,
                },
                &prepared_sub_writes,
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
        parent: AtomicCompositeParent<'_>,
        prepared_sub_writes: &[PreparedCompositeSubWrite],
    ) -> Result<bool, DispatchError> {
        let tenant = parent.tenant;
        let parent_entity_type = parent.entity_type;
        let parent_entity_id = parent.entity_id;
        let parent_action = parent.action;
        let parent_idempotency = parent.idempotency;

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
        let timing_enabled = prepared_sub_writes.len() >= 10;
        let total_started_at = timing_enabled.then(std::time::Instant::now);
        let parent_started_at = timing_enabled.then(std::time::Instant::now);

        if parent.record_event
            && !self
                .composite_event_already_persisted(
                    &store,
                    &parent_persistence_id,
                    parent_idempotency,
                )
                .await?
        {
            self.ensure_atomic_composite_stream(
                &mut streams,
                tenant,
                parent_entity_type,
                parent_entity_id,
                None,
                false,
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
        let parent_ms = parent_started_at.map(|started| started.elapsed().as_millis() as u64);

        let stage_started_at = timing_enabled.then(std::time::Instant::now);
        for write in prepared_sub_writes {
            let persistence_id = format!("{tenant}:{}:{}", write.entity_type, write.entity_id);
            self.ensure_atomic_composite_stream(
                &mut streams,
                tenant,
                &write.entity_type,
                &write.entity_id,
                write.preflight_target.as_ref(),
                write.uses_parent_gate && write.action == "Create",
            )
            .await?;

            let table = self.transition_table_for_dispatch(tenant, &write.entity_type)?;
            let cross_entity_booleans =
                if table_has_cross_entity_guards_for_action(&table, &write.action) {
                    self.resolve_cross_entity_guards(
                        tenant,
                        &write.entity_type,
                        &write.entity_id,
                        &write.action,
                    )
                    .await
                } else {
                    BTreeMap::new()
                };
            let stream = streams
                .get_mut(&persistence_id)
                .expect("stream inserted before processing sub-write");

            let incomplete_pack_object_repair =
                is_incomplete_existing_pack_object_create(write, stream);

            if should_skip_existing_pack_object_create(write, stream) {
                continue;
            }

            if !incomplete_pack_object_repair
                && stream
                    .state
                    .has_processed_idempotency_key(&write.idempotency_key)
            {
                continue;
            }

            validate_composite_ref_compare_and_set(
                parent_entity_type,
                parent_action,
                write,
                stream,
            )?;

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
        let stage_ms = stage_started_at.map(|started| started.elapsed().as_millis() as u64);

        let mut appends = Vec::new();
        for (persistence_id, stream) in streams
            .iter()
            .filter(|(_, stream)| !stream.events.is_empty())
        {
            let table = self.transition_table_for_dispatch(tenant, &stream.entity_type)?;
            appends.push(PersistenceAppend {
                persistence_id: persistence_id.clone(),
                expected_sequence: stream.expected_sequence,
                events: stream.events.clone(),
                key_rows: Some(crate::key_index::entity_key_rows(
                    &table.keys,
                    &stream.state.fields,
                )),
            });
        }
        if appends.is_empty() {
            return Ok(true);
        }

        let append_started_at = timing_enabled.then(std::time::Instant::now);
        store
            .append_batch(&appends)
            .await
            .map_err(composite_batch_persistence_error)?;
        let append_ms = append_started_at.map(|started| started.elapsed().as_millis() as u64);

        let projection_collect_started_at = timing_enabled.then(std::time::Instant::now);
        self.update_composite_query_projections(tenant, &streams)
            .await?;
        let projection_collect_ms =
            projection_collect_started_at.map(|started| started.elapsed().as_millis() as u64);

        let projection_write_started_at = timing_enabled.then(std::time::Instant::now);
        let projection_write_ms =
            projection_write_started_at.map(|started| started.elapsed().as_millis() as u64);

        let reload_started_at = timing_enabled.then(std::time::Instant::now);
        for stream in streams.values() {
            if stream.events.is_empty() {
                continue;
            }
            if !stream.target_existed {
                continue;
            }
            self.stop_and_remove_entity(tenant, &stream.entity_type, &stream.entity_id);
            if stream.state.status == "Deleted" {
                continue;
            }
            let reloaded = self
                .get_tenant_entity_state_authoritative(
                    tenant,
                    &stream.entity_type,
                    &stream.entity_id,
                )
                .await
                .map_err(|error| DispatchError::Internal(error.to_string()))?;
            if reloaded.is_none() {
                return Err(DispatchError::Internal(format!(
                    "composite batch committed {}:{} but failed to reload it",
                    stream.entity_type, stream.entity_id
                )));
            }
        }
        let reload_ms = reload_started_at.map(|started| started.elapsed().as_millis() as u64);
        if let Some(started) = total_started_at {
            tracing::info!(
                tenant = %tenant,
                parent_entity_type,
                parent_entity_id,
                parent_action,
                sub_writes = prepared_sub_writes.len(),
                streams = streams.len(),
                parent_ms = parent_ms.unwrap_or_default(),
                stage_ms = stage_ms.unwrap_or_default(),
                append_ms = append_ms.unwrap_or_default(),
                projection_collect_ms = projection_collect_ms.unwrap_or_default(),
                projection_write_ms = projection_write_ms.unwrap_or_default(),
                reload_ms = reload_ms.unwrap_or_default(),
                total_ms = started.elapsed().as_millis() as u64,
                "composite atomic batch timing"
            );
        }

        Ok(true)
    }

    async fn ensure_atomic_composite_stream(
        &self,
        streams: &mut BTreeMap<String, AtomicCompositeStream>,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        preflight_target: Option<&PreflightCompositeTarget>,
        suppress_bootstrap_event: bool,
    ) -> Result<(), DispatchError> {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        if streams.contains_key(&persistence_id) {
            return Ok(());
        }

        let (target_exists, mut state) = if let Some(target) = preflight_target {
            (target.target_existed, target.state.clone())
        } else {
            let table = self.transition_table_for_dispatch(tenant, entity_type)?;
            let target = self
                .get_tenant_entity_state_authoritative(tenant, entity_type, entity_id)
                .await
                .map_err(|error| DispatchError::Internal(error.to_string()))?;
            let (target_exists, state) = match target {
                Some(target) => (true, target.state),
                None => (
                    false,
                    synthetic_initial_state(entity_type, entity_id, &table),
                ),
            };
            (target_exists, state)
        };
        let expected_sequence = state.sequence_nr;
        let mut events = Vec::new();
        if !suppress_bootstrap_event
            && !target_exists
            && expected_sequence == 0
            && state.total_event_count == 0
        {
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
                target_existed: target_exists,
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
        let mut latest = store
            .read_latest_events(&[parent_persistence_id.to_string()])
            .await
            .map_err(|e| {
                DispatchError::Internal(format!(
                    "failed to read parent journal tail before CompositeEvent append: {e}"
                ))
            })?;
        if latest.len() != 1 {
            return Err(DispatchError::Internal(format!(
                "parent journal tail read returned {} rows, expected one",
                latest.len()
            )));
        }
        let Some(latest) = latest.pop().flatten() else {
            return Ok(false);
        };
        let budget = crate::entity_actor::types::MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY;
        let from_sequence = latest.sequence_nr.saturating_sub(budget as u64);
        let read_limit = budget.checked_add(1).ok_or_else(|| {
            DispatchError::Internal("composite idempotency read budget overflowed".to_string())
        })?;
        let envelopes = store
            .read_events_bounded(parent_persistence_id, from_sequence, read_limit)
            .await
            .map_err(|e| {
                DispatchError::Internal(format!(
                    "failed to read parent journal before CompositeEvent append: {e}"
                ))
            })?;
        validate_composite_idempotency_window(
            parent_persistence_id,
            from_sequence,
            latest.sequence_nr,
            budget,
            &envelopes,
        )?;
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
        metadata: &CompositeActionMetadata,
        composite_agent_ctx: &AgentContext,
        parent_idempotency: &str,
    ) -> Result<Vec<PreparedCompositeSubWrite>, DispatchError> {
        let sub_security_ctx = composite_agent_ctx.security_ctx.as_ref().ok_or_else(|| {
            DispatchError::Internal(
                "composite sub-write authorization requires a security context".to_string(),
            )
        })?;
        let mut prepared = Vec::with_capacity(sub_writes.len());
        let mut governed_cache = BTreeMap::new();

        for (idx, sub_write) in sub_writes.iter().cloned().enumerate() {
            let sub_entity_type = sub_write.entity_type.clone();
            let sub_entity_id = sub_write.entity_id.clone();
            let sub_action = sub_write.action.clone();
            let sub_params = normalize_sub_write_params(sub_write);

            let governed = match governed_cache.get(&sub_entity_type) {
                Some(governed) => *governed,
                None => {
                    let governed = self
                        .is_entity_type_governed(tenant, &sub_entity_type)
                        .map_err(DispatchError::Internal)?;
                    governed_cache.insert(sub_entity_type.clone(), governed);
                    governed
                }
            };
            if !governed {
                return Err(DispatchError::Ungoverned(sub_entity_type));
            }

            let use_parent_gate =
                composite_sub_write_uses_parent_gate(metadata, &sub_entity_type, &sub_action);

            prepared.push(PreparedCompositeSubWrite {
                idx,
                entity_type: sub_entity_type,
                entity_id: sub_entity_id,
                action: sub_action,
                params: sub_params,
                idempotency_key: format!(
                    "composite:{tenant}:{entity_type}:{entity_id}:{action}:{parent_idempotency}:subwrite:{idx}"
                ),
                preflight_target: None,
                uses_parent_gate: use_parent_gate,
            });
        }

        let known_absent_create_targets = self
            .composite_known_absent_create_targets(tenant, &prepared)
            .await?;

        for write in &mut prepared {
            let known_absent_create = known_absent_create_targets
                .contains(&(write.entity_type.clone(), write.entity_id.clone()));
            write.preflight_target = Some(
                self.load_composite_preflight_target(tenant, write, known_absent_create)
                    .await?,
            );
        }

        // Authorize against the exact state that transition preflight and the
        // later compare-and-set will use. In particular, a `Create` colliding
        // with a live target must see live Cedar attributes; only durable
        // absence is represented by synthetic initial attributes.
        for write in &prepared {
            if write.uses_parent_gate {
                continue;
            }
            let resource_attrs = self
                .composite_sub_write_auth_resource_attrs(tenant, write)
                .await?;
            self.authorize_with_context(
                sub_security_ctx,
                &write.action,
                &write.entity_type,
                &resource_attrs,
                tenant.as_str(),
            )
            .map_err(|denial| {
                DispatchError::AuthzDenied(format!(
                    "composite {entity_type}.{action} sub-write {} denied for {}.{}: {denial}",
                    write.idx, write.entity_type, write.action
                ))
            })?;
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

    async fn composite_known_absent_create_targets(
        &self,
        tenant: &TenantId,
        prepared: &[PreparedCompositeSubWrite],
    ) -> Result<BTreeSet<(String, String)>, DispatchError> {
        let Some(query_plane) = self.query_plane_store() else {
            return Ok(BTreeSet::new());
        };

        let mut by_type: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for write in prepared {
            if write.uses_parent_gate
                || write.action != "Create"
                || self.entity_exists(tenant, &write.entity_type, &write.entity_id)
            {
                continue;
            }
            by_type
                .entry(write.entity_type.clone())
                .or_default()
                .insert(write.entity_id.clone());
        }

        let mut absent = BTreeSet::new();
        for (entity_type, ids) in by_type {
            let entity_ids = ids.into_iter().collect::<Vec<_>>();
            let Some(rows) = query_plane
                .load_entity_catalog_rows(tenant.as_str(), &entity_type, &entity_ids)
                .await
                .map_err(|e| {
                    DispatchError::Internal(format!(
                        "query projection preflight failed for composite {entity_type} creates: {e}"
                    ))
                })?
            else {
                continue;
            };

            let present = rows
                .into_iter()
                .map(|row| row.entity_id)
                .collect::<BTreeSet<_>>();
            for entity_id in entity_ids {
                if !present.contains(&entity_id) {
                    absent.insert((entity_type.clone(), entity_id));
                }
            }
        }

        Ok(absent)
    }

    async fn load_composite_preflight_target(
        &self,
        tenant: &TenantId,
        write: &PreparedCompositeSubWrite,
        known_absent_create: bool,
    ) -> Result<PreflightCompositeTarget, DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, &write.entity_type)?;
        let known_absent_create = known_absent_create
            && write.action == "Create"
            && self.event_journal().is_none()
            && !self.entity_exists(tenant, &write.entity_type, &write.entity_id);
        let target = if known_absent_create {
            None
        } else {
            self.get_tenant_entity_state_authoritative(tenant, &write.entity_type, &write.entity_id)
                .await
                .map_err(|error| DispatchError::Internal(error.to_string()))?
        };
        let (target_exists, target_state) = match target {
            Some(target) => (true, target.state),
            None => (
                false,
                synthetic_initial_state(&write.entity_type, &write.entity_id, &table),
            ),
        };

        Ok(PreflightCompositeTarget {
            target_existed: target_exists,
            state: target_state,
        })
    }

    async fn preflight_composite_sub_write_transition(
        &self,
        tenant: &TenantId,
        parent_entity_type: &str,
        parent_action: &str,
        write: &PreparedCompositeSubWrite,
    ) -> Result<(), DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, &write.entity_type)?;
        let preflight_target = write.preflight_target.as_ref().ok_or_else(|| {
            DispatchError::Internal(format!(
                "composite {parent_entity_type}.{parent_action} sub-write {} has no preflight target",
                write.idx
            ))
        })?;
        let target_state = &preflight_target.state;

        if target_state.has_processed_idempotency_key(&write.idempotency_key) {
            return Ok(());
        }

        validate_composite_ref_preflight_compare_and_set(
            parent_entity_type,
            parent_action,
            write,
            preflight_target,
        )?;

        if !target_state.can_accept_event() {
            return Err(DispatchError::Internal(format!(
                "composite {parent_entity_type}.{parent_action} sub-write {} would exceed the event budget for {}:{}",
                write.idx, write.entity_type, write.entity_id
            )));
        }

        let cross_entity_booleans =
            if table_has_cross_entity_guards_for_action(&table, &write.action) {
                self.resolve_cross_entity_guards(
                    tenant,
                    &write.entity_type,
                    &write.entity_id,
                    &write.action,
                )
                .await
            } else {
                BTreeMap::new()
            };
        let eval_ctx = build_eval_context_with_xref(target_state, &cross_entity_booleans);
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
        write: &PreparedCompositeSubWrite,
    ) -> Result<BTreeMap<String, Value>, DispatchError> {
        let target = write.preflight_target.as_ref().ok_or_else(|| {
            DispatchError::Internal(format!(
                "composite sub-write {} has no authorization target",
                write.idx
            ))
        })?;
        if target.target_existed {
            return self
                .authz_resource_attrs_from_state(
                    tenant,
                    &write.entity_type,
                    &write.entity_id,
                    &target.state,
                )
                .await
                .map_err(DispatchError::Internal);
        }
        if write.action != "Create" {
            return Err(DispatchError::Internal(format!(
                "composite sub-write target {}:{} is not live for action '{}'",
                write.entity_type, write.entity_id, write.action
            )));
        }
        self.build_create_authz_resource_attrs(
            tenant,
            &write.entity_type,
            &write.entity_id,
            &write.params,
        )
        .await
        .map_err(DispatchError::Internal)
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
}

#[cfg(test)]
#[path = "composite_test.rs"]
mod tests;
