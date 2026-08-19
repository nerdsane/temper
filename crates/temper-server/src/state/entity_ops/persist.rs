//! Create, update, and delete entity state.

use super::helpers::{
    record_projection_update_error, record_projection_update_started,
    record_projection_update_success,
};
use crate::entity_actor::{EntityEvent, EntityResponse, EntityState, InProcessEntityRuntime};
use crate::events::EntityStateChange;
use crate::runtime_metrics;
use crate::state::ServerState;
use crate::state::dispatch::retry;
use crate::storage::DataOnlyCreateRecord;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope, PersistenceError};
use temper_runtime::plug::RuntimeRequest;
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use tracing::{Instrument, instrument};

impl ServerState {
    /// Spawn or load the actor, then return its current state.
    pub async fn get_or_create_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor_with_fields(tenant, entity_type, entity_id, initial_fields)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let policy = self.dispatch_retry_policy();
        let runtime = InProcessEntityRuntime::new(actor_ref);
        let response = retry::execute_with_backoff(&runtime, RuntimeRequest::GetState, &policy)
            .await
            .result
            .map_err(|e| format!("Actor query failed: {e}"))?;

        // Broadcast entity creation event for SSE subscribers
        let seq = self.next_entity_event_sequence(tenant.as_str(), entity_type, entity_id);
        let change = EntityStateChange {
            seq,
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: "Created".to_string(),
            status: response.state.status.clone(),
            tenant: tenant.to_string(),
            agent_id: None,
            session_id: None,
            intent: None,
            observation_metadata: None,
        };
        self.record_entity_observe_event_with_seq(
            tenant.as_str(),
            entity_type,
            entity_id,
            seq,
            "state_change",
            serde_json::to_value(&change).unwrap_or_default(),
        );
        let _ = self.event_tx.send(change);

        if let Some(query_plane) = self.query_plane_store() {
            let status = response.state.status.clone();
            let fields = self.query_projection_fields(tenant, entity_type, &response.state.fields);
            let projected_state = self.query_projection_state(&response.state);
            let sequence_nr = response.state.sequence_nr;
            let operation = "upsert";
            let source = "create";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            if let Err(e) = query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &status,
                    &fields,
                    &projected_state,
                    sequence_nr,
                )
                .await
            {
                record_projection_update_error(
                    tenant,
                    entity_type,
                    operation,
                    source,
                    projection_started_at,
                );
                // Surface the projection failure instead of swallowing it.
                // Previously this was a `warn` followed by `Ok(response)`,
                // which produced HTTP 201 even when the entity wasn't
                // discoverable via $filter — silently corrupting any caller
                // that does write-then-read-back. Returning Err propagates
                // up through the OData write handler to a 5xx so the client
                // knows to retry. The event is already durable in the
                // events table; on retry the actor's idempotent UPSERT into
                // entity_catalog/entity_field_index will succeed (or fail
                // again loudly).
                tracing::error!(
                    error = %e,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "failed to update query projection during create"
                );
                return Err(format!("query projection write failed during create: {e}"));
            }
            record_projection_update_success(
                tenant,
                entity_type,
                operation,
                source,
                sequence_nr,
                projection_started_at,
            );
        }

        Ok(response)
    }

    /// Create a durable data-only entity without spawning an actor.
    ///
    /// This path is only eligible for transition tables with no rules. It
    /// preserves the event journal, projection acknowledgement, in-memory
    /// entity index, and observe/SSE event contracts used by the actor path.
    #[instrument(skip_all, fields(otel.name = "entity.create_data_only_tenant_entity_fast_path", tenant = %tenant, entity_type, entity_id))]
    pub async fn try_create_data_only_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Result<Option<EntityResponse>, String> {
        if !initial_fields.is_object() {
            return Ok(None);
        }
        if self.is_pg_actor_backed(tenant, entity_type) {
            return Ok(None);
        }

        let table = {
            let reg = self.registry.read().unwrap();
            reg.get_table_live(tenant, entity_type)
        }
        .or_else(|| {
            self.transition_tables
                .get(entity_type)
                .map(|t| Arc::new(RwLock::new((**t).clone())))
        });
        let Some(table_ref) = table else {
            return Ok(None);
        };
        let table = table_ref
            .read()
            .expect("transition table lock poisoned")
            .clone();
        if !table.rules.is_empty() {
            return Ok(None);
        }

        let Some((store, backend)) = self.event_journal() else {
            return Ok(None);
        };
        let Some(query_plane) = self.query_plane_store() else {
            return Ok(None);
        };

        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let initial_fields =
            crate::entity_actor::effects::sanitize_action_params(&initial_fields).into_owned();
        let mut fields = initial_fields.clone();
        crate::entity_actor::effects::canonicalize_entity_fields(
            &mut fields,
            entity_id,
            &table.initial_state,
        );

        let mut state = EntityState {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            status: table.initial_state.clone(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields,
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        };

        let created = EntityEvent {
            action: "Created".to_string(),
            from_status: String::new(),
            to_status: state.status.clone(),
            timestamp: sim_now(),
            params: initial_fields,
            idempotency_key: None,
        };
        let payload = serde_json::to_value(&created)
            .map_err(|e| format!("failed to serialize Created event: {e}"))?;
        let envelope = PersistenceEnvelope {
            sequence_nr: 1,
            event_type: created.action.clone(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: created.timestamp,
                actor_id: persistence_id.clone(),
            },
        };

        let projection_fields = self.query_projection_fields(tenant, entity_type, &state.fields);
        let mut created_projection_state = state.clone();
        created_projection_state.sequence_nr = 1;
        created_projection_state.push_event_bounded(created.clone());
        let projection_state = self.query_projection_state(&created_projection_state);
        if let Some(native_store) = self.data_only_create_store() {
            let operation = "native_create";
            let source = "data_only_create_fast_path";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            let native_span = tracing::info_span!(
                "entity.data_only_create_native_storage",
                otel.name = "entity.data_only_create_native_storage",
                tenant = %tenant,
                entity_type,
                entity_id,
            );
            match native_store
                .create_data_only_entity(DataOnlyCreateRecord {
                    tenant: tenant.as_str(),
                    entity_type,
                    entity_id,
                    status: &state.status,
                    fields: &projection_fields,
                    state: &projection_state,
                    event: &envelope,
                })
                .instrument(native_span)
                .await
            {
                Ok(new_seq) => {
                    state.sequence_nr = new_seq;
                    record_projection_update_success(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        state.sequence_nr,
                        projection_started_at,
                    );
                }
                Err(PersistenceError::ConcurrencyViolation { .. }) => {
                    record_projection_update_error(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        projection_started_at,
                    );
                    return Ok(None);
                }
                Err(e) => {
                    record_projection_update_error(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        projection_started_at,
                    );
                    tracing::error!(
                        error = %e,
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "failed to update query projection during native data-only create fast path"
                    );
                    return Err(format!(
                        "native data-only create failed for {entity_type}:{entity_id}: {e}"
                    ));
                }
            }
        } else {
            let append_started_at = Instant::now(); // determinism-ok: production-only append wait metric
            let append_result = store
                .append(&persistence_id, state.sequence_nr, &[envelope])
                .await;
            runtime_metrics::record_event_store_append_wait(
                backend.as_str(),
                "append",
                append_started_at.elapsed(),
            );
            match append_result {
                Ok(new_seq) => {
                    state.sequence_nr = new_seq;
                }
                Err(PersistenceError::ConcurrencyViolation { .. }) => {
                    return Ok(None);
                }
                Err(e) => {
                    return Err(format!(
                        "failed to persist data-only Created event for {entity_type}:{entity_id}: {e}"
                    ));
                }
            }

            let operation = "upsert";
            let source = "data_only_create_fast_path";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            if let Err(e) = query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &state.status,
                    &projection_fields,
                    &projection_state,
                    state.sequence_nr,
                )
                .await
            {
                record_projection_update_error(
                    tenant,
                    entity_type,
                    operation,
                    source,
                    projection_started_at,
                );
                tracing::error!(
                    error = %e,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "failed to update query projection during data-only create fast path"
                );
                return Err(format!(
                    "query projection write failed during data-only create: {e}"
                ));
            }
            record_projection_update_success(
                tenant,
                entity_type,
                operation,
                source,
                state.sequence_nr,
                projection_started_at,
            );
        }
        state.push_event_bounded(created);

        {
            let index_key = format!("{tenant}:{entity_type}");
            let mut index = self.entity_index.write().unwrap();
            index
                .entry(index_key)
                .or_default()
                .insert(entity_id.to_string());
        }
        runtime_metrics::record_server_state_metrics(self);

        let seq = self.next_entity_event_sequence(tenant.as_str(), entity_type, entity_id);
        let change = EntityStateChange {
            seq,
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: "Created".to_string(),
            status: state.status.clone(),
            tenant: tenant.to_string(),
            agent_id: None,
            session_id: None,
            intent: None,
            observation_metadata: None,
        };
        self.record_entity_observe_event_with_seq(
            tenant.as_str(),
            entity_type,
            entity_id,
            seq,
            "state_change",
            serde_json::to_value(&change).unwrap_or_default(),
        );
        let _ = self.event_tx.send(change);

        Ok(Some(EntityResponse {
            success: true,
            state,
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        }))
    }

    /// Update fields on an existing entity.
    #[instrument(skip_all, fields(otel.name = "entity.update_tenant_entity_fields", tenant = %tenant, entity_type, entity_id))]
    pub async fn update_tenant_entity_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
    ) -> Result<EntityResponse, String> {
        let response = self
            .update_tenant_entity_fields_checked(
                tenant,
                entity_type,
                entity_id,
                fields,
                replace,
                None,
            )
            .await?;
        if response.success {
            Ok(response)
        } else {
            Err(response
                .error
                .clone()
                .unwrap_or_else(|| "entity field update was rejected".to_string()))
        }
    }

    /// Update fields only if the actor still matches the state that Cedar
    /// authorized. The comparison and mutation are serialized by the actor
    /// mailbox, closing the read/authorize/write race.
    pub(crate) async fn update_tenant_entity_fields_if_current(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        expected_precondition: String,
    ) -> Result<EntityResponse, String> {
        self.update_tenant_entity_fields_checked(
            tenant,
            entity_type,
            entity_id,
            fields,
            replace,
            Some(expected_precondition),
        )
        .await
    }

    async fn update_tenant_entity_fields_checked(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        expected_precondition: Option<String>,
    ) -> Result<EntityResponse, String> {
        if !fields.is_object() {
            return Err("entity field update must be a JSON object".to_string());
        }
        let actor_ref = self
            .get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let mut policy = self.dispatch_retry_policy();
        if expected_precondition.is_some() {
            // A timed-out compare-and-set may already have committed. Retrying
            // the same precondition would turn that ambiguity into a false
            // conflict, so the caller must re-read and re-authorize instead.
            policy.max_attempts = 1;
        }
        let runtime = InProcessEntityRuntime::new(actor_ref);
        let response = retry::execute_with_backoff(
            &runtime,
            RuntimeRequest::UpdateFields {
                fields,
                replace,
                expected_precondition,
            },
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor update failed: {e}"))?;

        if response.success
            && let Some(query_plane) = self.query_plane_store()
        {
            let status = response.state.status.clone();
            let fields = self.query_projection_fields(tenant, entity_type, &response.state.fields);
            let projected_state = self.query_projection_state(&response.state);
            let sequence_nr = response.state.sequence_nr;
            let operation = "upsert";
            let source = "field_update";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            if let Err(e) = query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &status,
                    &fields,
                    &projected_state,
                    sequence_nr,
                )
                .await
            {
                record_projection_update_error(
                    tenant,
                    entity_type,
                    operation,
                    source,
                    projection_started_at,
                );
                // Same reasoning as create: don't ack a write that won't be
                // visible via $filter. Propagate so OData returns 5xx and
                // clients can retry against the idempotent upsert.
                tracing::error!(
                    error = %e,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "failed to update query projection during field update"
                );
                return Err(format!("query projection write failed during update: {e}"));
            }
            record_projection_update_success(
                tenant,
                entity_type,
                operation,
                source,
                sequence_nr,
                projection_started_at,
            );
        }

        Ok(response)
    }

    /// Delete an entity.
    #[instrument(skip_all, fields(otel.name = "entity.delete_tenant_entity", tenant = %tenant, entity_type, entity_id))]
    pub async fn delete_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<EntityResponse, String> {
        let response = self
            .delete_tenant_entity_checked(tenant, entity_type, entity_id, None)
            .await?;
        if response.success {
            Ok(response)
        } else {
            Err(response
                .error
                .clone()
                .unwrap_or_else(|| "entity delete was rejected".to_string()))
        }
    }

    /// Delete only if the actor still matches the state Cedar authorized.
    pub(crate) async fn delete_tenant_entity_if_current(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        expected_authorization_precondition: String,
    ) -> Result<EntityResponse, String> {
        self.delete_tenant_entity_checked(
            tenant,
            entity_type,
            entity_id,
            Some(expected_authorization_precondition),
        )
        .await
    }

    async fn delete_tenant_entity_checked(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        expected_authorization_precondition: Option<String>,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let mut policy = self.dispatch_retry_policy();
        if expected_authorization_precondition.is_some() {
            policy.max_attempts = 1;
        }
        let runtime = InProcessEntityRuntime::new(actor_ref.clone());
        let response = retry::execute_with_backoff(
            &runtime,
            RuntimeRequest::Delete {
                expected_authorization_precondition,
            },
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor delete failed: {e}"))?;

        if response.success {
            if let Some(query_plane) = self.query_plane_store() {
                let operation = "remove";
                let source = "delete";
                record_projection_update_started(tenant, entity_type, operation, source);
                let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
                if let Err(e) = query_plane
                    .remove_projection(tenant.as_str(), entity_type, entity_id)
                    .await
                {
                    record_projection_update_error(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        projection_started_at,
                    );
                    // Delete is idempotent against the projection: a stale row
                    // surviving in entity_field_index after the tombstone is a
                    // visibility leak, not data corruption. Log loudly so it's
                    // greppable but don't block the delete from completing —
                    // the actor and event journal already reflect the tombstone.
                    tracing::error!(
                        error = %e,
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "failed to remove query projection during delete (stale projection row will linger until next successful upsert/delete)"
                    );
                } else {
                    record_projection_update_success(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        response.state.sequence_nr,
                        projection_started_at,
                    );
                }
            }

            // Tombstone persisted successfully; now it is safe to remove actor
            // and in-memory index entries.
            let _ = actor_ref.stop();
            self.remove_entity(tenant, entity_type, entity_id);
        }

        Ok(response)
    }
}
