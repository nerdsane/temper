use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use temper_authz::SecurityContext;
use temper_jit::table::{CompositeActionMetadata, TransitionTable};
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, CompositeEvent, CompositeEventSubWrite, EventMetadata, PersistenceAppend,
    PersistenceBatchIdempotency, PersistenceEnvelope, PersistenceError, SnapshotSourceFence,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::effects::{
    FieldSyncMode, build_eval_context_with_xref, process_action_with_xref_and_field_mode,
};
use crate::entity_actor::{
    EntityRecoveryContext, EntityResponse, EntityState, recover_entity_state_from_stable_sources,
    state_materialization_envelope,
};
use crate::request_context::AgentContext;
use crate::state::account_verification::CommonsAccountVerificationError;
use crate::state::app_uniqueness::CommonsAppUniquenessError;
use crate::state::storage_caps::{CommonsStorageCapError, CommonsStorageWrite};
use crate::storage::BackendLabel;

use super::DispatchError;
use super::effects::PostDispatchContext;

mod atomic;
mod helpers;
mod preflight;
mod projection;
use helpers::*;

const MAX_ACTIVE_COMPOSITE_CLAIM_LOCKS: usize = 4_096;

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

#[derive(Clone, Copy)]
struct AtomicCompositeParent<'a> {
    tenant: &'a TenantId,
    entity_type: &'a str,
    entity_id: &'a str,
    action: &'a str,
    idempotency: &'a str,
    record_event: bool,
    agent_ctx: &'a AgentContext,
}

struct AtomicCompositePostCommit {
    entity_type: String,
    entity_id: String,
    action: String,
    params: Value,
    idempotency_key: String,
    response: EntityResponse,
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
    /// Exact transition table captured before any composite recovery/evaluation.
    table: TransitionTable,
    target_existed: bool,
    state: EntityState,
    expected_sequence: u64,
    snapshot_source: SnapshotSourceFence,
    materialization_baseline: Option<EntityState>,
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

fn prepare_composite_intent_sub_writes(
    parent: AtomicCompositeParent<'_>,
    sub_writes: &[CompositeSubWrite],
    metadata: &CompositeActionMetadata,
) -> Vec<PreparedCompositeSubWrite> {
    sub_writes
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, sub_write)| {
            let entity_type = sub_write.entity_type.clone();
            let entity_id = sub_write.entity_id.clone();
            let action = sub_write.action.clone();
            PreparedCompositeSubWrite {
                idx,
                entity_type: entity_type.clone(),
                entity_id,
                action: action.clone(),
                params: normalize_sub_write_params(sub_write),
                idempotency_key: format!(
                    "composite:{}:{}:{}:{}:{}:subwrite:{idx}",
                    parent.tenant,
                    parent.entity_type,
                    parent.entity_id,
                    parent.action,
                    parent.idempotency
                ),
                preflight_target: None,
                uses_parent_gate: composite_sub_write_uses_parent_gate(
                    metadata,
                    &entity_type,
                    &action,
                ),
            }
        })
        .collect()
}

fn composite_batch_claim(
    parent: AtomicCompositeParent<'_>,
    prepared_sub_writes: &[PreparedCompositeSubWrite],
) -> Result<PersistenceBatchIdempotency, DispatchError> {
    let persistence_id = format!(
        "{}:{}:{}",
        parent.tenant, parent.entity_type, parent.entity_id
    );
    let composite_event = build_composite_event(
        parent.tenant,
        parent.entity_type,
        parent.entity_id,
        parent.action,
        parent.idempotency,
        prepared_sub_writes,
    );
    let intent_sub_writes = prepared_sub_writes
        .iter()
        .map(|write| {
            (
                write.idx,
                write.entity_type.as_str(),
                write.entity_id.as_str(),
                write.action.as_str(),
                canonical_json(&write.params),
                write.idempotency_key.as_str(),
                write.uses_parent_gate,
            )
        })
        .collect::<Vec<_>>();
    let intent_bytes =
        serde_json::to_vec(&(parent.record_event, composite_event, intent_sub_writes)).map_err(
            |error| {
                DispatchError::Internal(format!(
                    "failed to encode composite batch idempotency intent: {error}"
                ))
            },
        )?;
    let intent_hash = format!("{:x}", Sha256::digest(intent_bytes));
    let repairs_incomplete_pack_payload = prepared_sub_writes.iter().any(|write| {
        write.uses_parent_gate
            && write.action == "Create"
            && is_pack_object_entity(&write.entity_type)
            && !has_complete_git_object_payload(&write.params)
    });
    let idempotency_key = if repairs_incomplete_pack_payload {
        format!("{}:partial:{intent_hash}", parent.idempotency)
    } else {
        parent.idempotency.to_string()
    };
    Ok(PersistenceBatchIdempotency {
        persistence_id,
        idempotency_key,
        intent_hash,
    })
}

fn composite_claim_lock_key(claim: &PersistenceBatchIdempotency) -> String {
    debug_assert!(!claim.persistence_id.is_empty());
    debug_assert!(!claim.idempotency_key.is_empty());
    format!(
        "{}:{}{}:{}",
        claim.persistence_id.len(),
        claim.persistence_id,
        claim.idempotency_key.len(),
        claim.idempotency_key
    )
}

impl crate::state::ServerState {
    fn effectful_composite_reservation_persistence_id(
        parent: AtomicCompositeParent<'_>,
        intended: &CompositeEvent,
    ) -> String {
        let mut digest = Sha256::new();
        for component in [
            parent.entity_type,
            parent.entity_id,
            intended.composite_idempotency_key.as_str(),
        ] {
            digest.update((component.len() as u64).to_be_bytes());
            digest.update(component.as_bytes());
        }
        format!("{}:_CompositeIntent:{:x}", parent.tenant, digest.finalize())
    }

    async fn effectful_composite_reservation_exists(
        &self,
        store: &crate::storage::BoxedEventStore,
        parent: AtomicCompositeParent<'_>,
        intended: &CompositeEvent,
    ) -> Result<bool, DispatchError> {
        let persistence_id = Self::effectful_composite_reservation_persistence_id(parent, intended);
        let events = store
            .read_events(&persistence_id, 0)
            .await
            .map_err(composite_batch_persistence_error)?;
        let mut existing: Option<CompositeEvent> = None;
        for event in events {
            if event.event_type != COMPOSITE_EVENT_TYPE {
                continue;
            }
            let decoded =
                serde_json::from_value::<CompositeEvent>(event.payload).map_err(|error| {
                    DispatchError::Internal(format!(
                        "malformed composite reservation at sequence {}: {error}",
                        event.sequence_nr
                    ))
                })?;
            if decoded.composite_idempotency_key != intended.composite_idempotency_key {
                continue;
            }
            if existing.as_ref().is_some_and(|prior| prior != &decoded) {
                return Err(DispatchError::Internal(format!(
                    "composite idempotency key '{}' has conflicting durable reservations",
                    parent.idempotency
                )));
            }
            existing = Some(decoded);
        }
        let Some(existing) = existing else {
            return Ok(false);
        };
        if existing != *intended {
            return Err(DispatchError::Internal(format!(
                "composite idempotency key '{}' was reused with a different intent",
                parent.idempotency
            )));
        }
        Ok(true)
    }

    async fn persist_effectful_composite_reservation(
        &self,
        store: &crate::storage::BoxedEventStore,
        parent: AtomicCompositeParent<'_>,
        intended: &CompositeEvent,
    ) -> Result<(), DispatchError> {
        if self
            .effectful_composite_reservation_exists(store, parent, intended)
            .await?
        {
            return Ok(());
        }
        let persistence_id = Self::effectful_composite_reservation_persistence_id(parent, intended);
        let boundary = store
            .journal_boundary(&persistence_id)
            .await
            .map_err(composite_batch_persistence_error)?;
        let envelope = composite_event_envelope(&persistence_id, intended)?;
        if let Err(error) = store
            .append(&persistence_id, boundary.latest_sequence, &[envelope])
            .await
            && !self
                .effectful_composite_reservation_exists(store, parent, intended)
                .await?
        {
            return Err(composite_batch_persistence_error(error));
        }

        Ok(())
    }

    async fn acquire_composite_claim_lock(
        &self,
        claim: &PersistenceBatchIdempotency,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, DispatchError> {
        let key = composite_claim_lock_key(claim);
        let serializer = {
            let mut locks = self.composite_claim_locks.lock().map_err(|error| {
                DispatchError::Internal(format!(
                    "composite claim serializer lock poisoned: {error}"
                ))
            })?;
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
                lock
            } else {
                if locks.len() >= MAX_ACTIVE_COMPOSITE_CLAIM_LOCKS {
                    return Err(DispatchError::Deferred { retry_after_ms: 1 });
                }
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        Ok(serializer.lock_owned().await)
    }

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

        let composite_action_context = format!("composite:{entity_type}.{action}");
        let mut composite_agent_ctx = agent_ctx.clone();
        composite_agent_ctx.security_ctx = Some(
            agent_ctx
                .security_ctx
                .clone()
                .unwrap_or_else(|| SecurityContext::from_headers(&[]))
                .with_action_context(composite_action_context),
        );

        let parent = AtomicCompositeParent {
            tenant,
            entity_type,
            entity_id,
            action,
            idempotency: &parent_idempotency,
            record_event: metadata.record_parent_event,
            agent_ctx: &composite_agent_ctx,
        };
        let intent_prepared = prepare_composite_intent_sub_writes(parent, &sub_writes, &metadata);
        let batch_claim = composite_batch_claim(parent, &intent_prepared)?;
        // The durable append is still the final cross-backend authority, but the
        // current runtime is single-process. Serialize one raw batch namespace so
        // an identical callback cannot observe "claim absent", lose a
        // state-dependent preflight race to its peer, and return a conflict before
        // reaching the backend's exact-claim replay.
        let _claim_lock = self.acquire_composite_claim_lock(&batch_claim).await?;
        let event_journal = self.event_journal();
        let batch_already_committed = match event_journal.as_ref() {
            Some((store, _)) => store
                .batch_idempotency_committed(&batch_claim)
                .await
                .map_err(composite_batch_persistence_error)?,
            None => false,
        };

        let _commons_guardrail_lock = self.acquire_commons_write_guardrail_lock(tenant).await;
        if batch_already_committed {
            if !self
                .apply_composite_sub_writes_atomic(parent, &intent_prepared, batch_claim, true)
                .await?
            {
                return Err(DispatchError::Internal(
                    "committed composite batch cannot be repaired without its durable event journal"
                        .to_string(),
                ));
            }
            return Ok(true);
        }

        let mut intended_reservation = build_composite_event(
            tenant,
            entity_type,
            entity_id,
            action,
            &parent_idempotency,
            &intent_prepared,
        );
        intended_reservation
            .intent_hash
            .clone_from(&batch_claim.intent_hash);
        intended_reservation
            .composite_idempotency_key
            .clone_from(&batch_claim.idempotency_key);
        if let Some((store, _)) = event_journal.as_ref() {
            self.effectful_composite_reservation_exists(store, parent, &intended_reservation)
                .await?;
        }

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
                parent,
                &prepared_sub_writes,
                batch_claim.clone(),
                false,
            )
            .await?
        {
            return Ok(true);
        }

        if let Some((store, _)) = event_journal.as_ref() {
            self.persist_effectful_composite_reservation(store, parent, &intended_reservation)
                .await?;
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
}

#[cfg(test)]
#[path = "composite_test.rs"]
mod tests;
