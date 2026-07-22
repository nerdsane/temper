use std::sync::{Arc, RwLock};

use temper_runtime::persistence::{IndexReconciliation, PersistenceError};
use temper_runtime::scheduler::sim_now;

use crate::entity_actor::{EntityEvent, EntityResponse};
use crate::events::EntityStateChange;

use super::file_writes::content_hash_and_native_blob_key;
use super::{FileStreamContentError, ServerState};

mod synthetic;

use synthetic::{
    apply_synthetic_file_action, initial_file_state, push_synthetic_event, synthetic_envelope,
};

impl ServerState {
    /// Create a brand-new TemperFS `File` and persist its first stream bytes as
    /// one kernel operation.
    ///
    /// This avoids exposing a durable/projection-visible File row whose
    /// `$value` has no backing blob yet. Callers must use the normal
    /// `StreamUpdated` dispatch path for existing Files.
    pub async fn create_file_with_initial_stream_content(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        create_params: serde_json::Value,
        body: &[u8],
        mime_type: &str,
        agent_ctx: &crate::request_context::AgentContext,
    ) -> Result<crate::entity_actor::EntityResponse, String> {
        self.create_file_with_initial_stream_content_checked(
            tenant,
            file_id,
            create_params,
            body,
            mime_type,
            agent_ctx,
        )
        .await
        .map_err(|error| error.to_string())
    }

    #[tracing::instrument(skip_all, fields(
        otel.name = "state.create_file_with_initial_stream_content",
        tenant = %tenant,
        file_id,
        request_bytes = body.len() as u64,
    ))]
    pub(crate) async fn create_file_with_initial_stream_content_checked(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        create_params: serde_json::Value,
        body: &[u8],
        mime_type: &str,
        agent_ctx: &crate::request_context::AgentContext,
    ) -> Result<crate::entity_actor::EntityResponse, FileStreamContentError> {
        if self
            .ensure_entity_loaded_in_generation(tenant, "File", file_id, agent_ctx)
            .await
        {
            return Err(FileStreamContentError::ActionRejected(format!(
                "File('{file_id}') already exists; use File $value update for existing content"
            )));
        }

        let Some((store, _backend)) = self.event_journal() else {
            return Err(FileStreamContentError::State(
                "File initial content create requires an event journal".to_string(),
            ));
        };
        let table = self.file_transition_table(tenant)?;

        // Reject a brand-new File whose target Workspace is not Active BEFORE
        // persisting any bytes. `workspace_id` arrives in the create params on
        // this path (the File has no persisted state yet).
        let workspace_id = create_params
            .get("workspace_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        self.reject_write_if_workspace_not_active(tenant, &workspace_id, agent_ctx)
            .await?;

        let (content_hash, blob_key) = content_hash_and_native_blob_key(body);
        self.put_content_addressed_blob(tenant, &blob_key, body, None)
            .await
            .map_err(|e| {
                FileStreamContentError::BlobStore(format!(
                    "failed to persist blob '{blob_key}': {e}"
                ))
            })?;

        let persistence_id = format!("{tenant}:File:{file_id}");
        let mut state = initial_file_state(file_id, &table, serde_json::json!({}));
        let mut events = Vec::with_capacity(3);

        let created = EntityEvent {
            action: "Created".to_string(),
            from_status: String::new(),
            to_status: state.status.clone(),
            timestamp: sim_now(),
            params: serde_json::json!({}),
            idempotency_key: None,
        };
        push_synthetic_event(&mut state, &mut events, created);

        let no_xref = std::collections::BTreeMap::new();
        if create_params
            .as_object()
            .is_some_and(|params| !params.is_empty())
        {
            let create_event =
                apply_synthetic_file_action(&mut state, &table, "Create", create_params, &no_xref)?;
            push_synthetic_event(&mut state, &mut events, create_event);
        }

        let version_number = state.counters.get("version_count").copied().unwrap_or(0) + 1;
        let previous_version_id = state
            .fields
            .get("last_version_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let created_by = agent_ctx.agent_id.clone().unwrap_or_default();
        let stream_params = serde_json::json!({
            "content_hash": content_hash,
            "size_bytes": body.len() as i64,
            "mime_type": mime_type,
            "version_number": version_number,
            "previous_version_id": previous_version_id,
            "created_by": created_by,
        });
        // The `File.StreamUpdated` spec guard requires the owning Workspace to
        // be Active. On the live dispatch path that boolean is resolved by
        // `resolve_cross_entity_guards`; this atomic synthetic path bypasses
        // that resolution, so supply the `__xref` boolean directly. We already
        // confirmed the workspace is Active (or vacuous) above, so the guard
        // holds — pass `true`.
        let mut stream_xref = std::collections::BTreeMap::new();
        stream_xref.insert("__xref:Workspace:workspace_id".to_string(), true);
        let stream_event = apply_synthetic_file_action(
            &mut state,
            &table,
            "StreamUpdated",
            stream_params,
            &stream_xref,
        )?;
        push_synthetic_event(&mut state, &mut events, stream_event);

        let envelopes = events
            .iter()
            .enumerate()
            .map(|(idx, event)| synthetic_envelope(&persistence_id, (idx + 1) as u64, event))
            .collect::<Result<Vec<_>, _>>()?;

        // Exact reconciliation participates even for a no-key contract so a
        // hot spec change cannot strand ownership rows from the prior version.
        let reconcile_keys = true;
        let key_rows = crate::key_index::derive_entity_key_rows(
            &table.keys,
            &state.fields,
            state.status != "Deleted",
        );
        let key_set_signature = crate::key_index::declared_key_write_contract(&table);
        match store
            .append_with_index_rows(
                &persistence_id,
                0,
                &envelopes,
                &key_rows,
                &[],
                IndexReconciliation {
                    keys: reconcile_keys,
                    key_set_signature: Some(key_set_signature),
                    vectors: false,
                    snapshot_source: temper_runtime::persistence::SnapshotSourceFence::Absent,
                },
            )
            .await
        {
            Ok(sequence_nr) => state.sequence_nr = sequence_nr,
            Err(
                PersistenceError::ConcurrencyViolation { .. }
                | PersistenceError::SnapshotGenerationChanged,
            ) => {
                return Err(FileStreamContentError::ActionRejected(format!(
                    "File('{file_id}') already exists; use File $value update for existing content"
                )));
            }
            Err(error) => {
                return Err(FileStreamContentError::State(format!(
                    "failed to persist atomic File initial content events for File('{file_id}'): {error}"
                )));
            }
        }

        if let Some(query_plane) = self.query_plane_store() {
            let fields = self.query_projection_fields(tenant, "File", &state.fields);
            let projected_state = self.query_projection_state(&state);
            query_plane
                .upsert_projection(
                    tenant.as_str(),
                    "File",
                    file_id,
                    &state.status,
                    &fields,
                    &projected_state,
                    state.sequence_nr,
                )
                .await
                .map_err(|error| {
                    FileStreamContentError::State(format!(
                        "query projection write failed during atomic File initial content create: {error}"
                    ))
                })?;
        }

        self.stop_and_remove_entity(tenant, "File", file_id);
        {
            let index_key = format!("{tenant}:File");
            let mut index = self.entity_index.write().unwrap();
            index
                .entry(index_key)
                .or_default()
                .insert(file_id.to_string());
        }
        crate::runtime_metrics::record_server_state_metrics(self);

        let response = EntityResponse {
            success: true,
            state,
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        };
        self.broadcast_synthetic_file_stream_update(tenant, file_id, &response, agent_ctx);
        Ok(response)
    }

    fn file_transition_table(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
    ) -> Result<temper_jit::table::TransitionTable, FileStreamContentError> {
        let table = {
            let reg = self.registry.read().unwrap();
            reg.get_table_live(tenant, "File")
        }
        .or_else(|| {
            self.transition_tables
                .get("File")
                .map(|t| Arc::new(RwLock::new((**t).clone())))
        })
        .ok_or_else(|| {
            FileStreamContentError::State(format!(
                "No transition table for tenant '{tenant}', entity type 'File'"
            ))
        })?;

        Ok(table
            .read()
            .expect("File transition table lock poisoned")
            .clone())
    }

    /// Build the Cedar resource attributes for a not-yet-existing `File`
    /// WITHOUT spawning its actor.
    ///
    /// Produces exactly what `load_authz_resource_snapshot` would for a
    /// freshly-spawned File so Cedar cannot allow/deny differently between a
    /// create-via-`$value` and an update on a fresh File:
    /// - `id` / `status` (lowercase) — the explicit inserts
    ///   `load_authz_resource_snapshot` adds from the entity id and the File
    ///   spec's initial state;
    /// - `Id` / `Status` (capitalized) — the fields `build_initial_state`
    ///   seeds (actor.rs) and that the snapshot then copies out of
    ///   `state.fields`;
    /// - `has_spec=true`.
    ///
    /// A freshly-spawned File has no other persisted fields, so the maps match.
    /// Avoiding the spawn is the point: it skips persisting the empty bootstrap
    /// `Created` event the spawn path (actor `pre_start`) would write — the
    /// ARN-87 write-side empty-`$value` window this closes.
    pub(crate) fn synthesize_new_file_resource_attrs(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, String> {
        let table = self
            .file_transition_table(tenant)
            .map_err(|error| error.to_string())?;
        let initial_state = serde_json::Value::String(table.initial_state.clone());
        let file_id_value = serde_json::Value::String(file_id.to_string());
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("id".to_string(), file_id_value.clone());
        attrs.insert("status".to_string(), initial_state.clone());
        attrs.insert("Id".to_string(), file_id_value);
        attrs.insert("Status".to_string(), initial_state);
        let has_spec = self
            .has_registered_spec(tenant, "File")
            .map_err(|error| error.to_string())?;
        attrs.insert("has_spec".to_string(), serde_json::Value::Bool(has_spec));
        Ok(attrs)
    }

    fn broadcast_synthetic_file_stream_update(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        response: &EntityResponse,
        agent_ctx: &crate::request_context::AgentContext,
    ) {
        self.metrics
            .record_transition("File", "StreamUpdated", true);

        let seq = self.next_entity_event_sequence(tenant.as_str(), "File", file_id);
        let change = EntityStateChange {
            seq,
            entity_type: "File".to_string(),
            entity_id: file_id.to_string(),
            action: "StreamUpdated".to_string(),
            status: response.state.status.clone(),
            tenant: tenant.to_string(),
            agent_id: agent_ctx.agent_id.clone(),
            session_id: agent_ctx.session_id.clone(),
            intent: agent_ctx.intent.clone(),
            observation_metadata: (!agent_ctx.observation_metadata.is_empty())
                .then(|| agent_ctx.observation_metadata.clone()),
        };
        self.record_entity_observe_event_with_seq(
            tenant.as_str(),
            "File",
            file_id,
            seq,
            "state_change",
            serde_json::to_value(&change).unwrap_or_default(),
        );
        let _ = self.event_tx.send(change);

        let dispatcher = self
            .reaction_dispatcher
            .read()
            .ok()
            .and_then(|slot| slot.clone());
        if let Some(dispatcher) = dispatcher {
            let state = self.clone();
            let tenant = tenant.clone();
            let file_id = file_id.to_string();
            let to_state = response.state.status.clone();
            let fields = response.state.fields.clone();
            let agent_ctx = agent_ctx
                .fork_tenant_generation_lease(&tenant)
                .unwrap_or_else(|| {
                    let mut detached = agent_ctx.clone();
                    detached.detach_tenant_generation_lease();
                    detached
                });
            tokio::spawn(async move {
                // determinism-ok: post-commit reaction side effects mirror the
                // existing non-awaited File `$value` update behavior.
                let Some(agent_ctx) = state
                    .activate_immediate_tenant_work(&tenant, agent_ctx)
                    .await
                else {
                    tracing::error!(
                        tenant = %tenant,
                        entity_id = %file_id,
                        "File initial-write reactions could not enter a complete runtime generation"
                    );
                    return;
                };
                dispatcher
                    .dispatch_reactions(
                        &state,
                        &tenant,
                        "File",
                        &file_id,
                        "StreamUpdated",
                        &to_state,
                        &fields,
                        0,
                        &agent_ctx,
                    )
                    .await;
            });
        }
    }
}
