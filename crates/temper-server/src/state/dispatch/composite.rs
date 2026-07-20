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
use crate::entity_actor::event_persistence::{
    PersistedStateTimeoutClock, apply_state_timeout_clock, encode_entity_event_payload,
};
use crate::request_context::AgentContext;
use crate::state::account_verification::CommonsAccountVerificationError;
use crate::state::app_uniqueness::CommonsAppUniquenessError;
use crate::state::storage_caps::{CommonsStorageCapError, CommonsStorageWrite};
use crate::storage::BackendLabel;

use super::DispatchError;

mod helpers;
mod preflight;
mod projection;
mod reconciliation;
mod streams;
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

#[derive(Debug, Clone)]
struct CompositeCreateAuthDefaults {
    initial_state: String,
    has_spec: bool,
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
                    None,
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
            let event_version = stream
                .state
                .sequence_nr
                .checked_add(1)
                .expect("composite event sequence overflow");
            let (envelope, clock) = composite_envelope(
                &persistence_id,
                &table,
                &stream.state,
                &event,
                event_version,
            )?;
            stream.events.push(envelope);
            stream.state.sequence_nr = event_version;
            apply_state_timeout_clock(&mut stream.state, clock);
            stream.state.push_event_bounded(event);
        }
        let stage_ms = stage_started_at.map(|started| started.elapsed().as_millis() as u64);

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

        let append_started_at = timing_enabled.then(std::time::Instant::now);
        store
            .append_batch(&appends)
            .await
            .map_err(composite_batch_persistence_error)?;
        let append_ms = append_started_at.map(|started| started.elapsed().as_millis() as u64);
        let stream_count = streams.len();
        let reconciliation_state = self.clone();
        let reconciliation_tenant = tenant.to_owned();
        // The journal commit is the point of no return. Giving the complete
        // post-commit state to an owned task before the next await means caller
        // cancellation cannot strand stale actors, timeout ownership, indexes,
        // or query projections. This bounded task performs no transition
        // evaluation and preserves deterministic per-stream BTreeMap order.
        let reconciliation = tokio::spawn(async move {
            // determinism-ok: cancellation-safe post-commit durability reconciliation
            reconciliation_state
                .reconcile_committed_composite_streams(
                    &reconciliation_tenant,
                    streams,
                    timing_enabled,
                )
                .await
        });
        let reconciliation_timing = reconciliation.await.map_err(|error| {
            DispatchError::Internal(format!(
                "composite batch committed but post-commit reconciliation task failed: {error}"
            ))
        })??;

        if let Some(started) = total_started_at {
            tracing::info!(
                tenant = %tenant,
                parent_entity_type,
                parent_entity_id,
                parent_action,
                sub_writes = prepared_sub_writes.len(),
                streams = stream_count,
                parent_ms = parent_ms.unwrap_or_default(),
                stage_ms = stage_ms.unwrap_or_default(),
                append_ms = append_ms.unwrap_or_default(),
                projection_collect_ms = reconciliation_timing.projection_ms.unwrap_or_default(),
                projection_write_ms = 0_u64,
                reload_ms = reconciliation_timing.reload_ms.unwrap_or_default(),
                total_ms = started.elapsed().as_millis() as u64,
                "composite atomic batch timing"
            );
        }

        Ok(true)
    }
}

#[cfg(test)]
#[path = "composite_test.rs"]
mod tests;

#[cfg(all(test, feature = "sim"))]
#[path = "composite_timeout_clock_tests.rs"]
mod timeout_clock_tests;
