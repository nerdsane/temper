use std::sync::{Arc, RwLock};

use temper_runtime::persistence::{
    AppendProjection, CommitRows, EventMetadata, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::sim_uuid;
use tracing::Instrument;

use crate::entity_actor::{EntityEvent, EntityResponse, EntityState, process_action_with_xref};
use crate::events::EntityStateChange;

use super::file_writes::content_hash_and_native_blob_key;
use super::{FileStreamContentError, ServerState, StoredFileContent};

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
        let workspace_id = create_params
            .get("workspace_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        self.check_new_file_writable(tenant, file_id, &workspace_id)
            .await?;

        let (content_hash, blob_key) = content_hash_and_native_blob_key(body);
        self.put_file_content_blob(tenant, &blob_key, body)
            .await
            .map_err(|e| {
                FileStreamContentError::BlobStore(format!(
                    "failed to persist blob '{blob_key}': {e}"
                ))
            })?;
        let content = StoredFileContent {
            content_hash,
            size_bytes: body.len() as u64,
        };
        self.commit_new_file_content(
            tenant,
            file_id,
            create_params,
            &content,
            mime_type,
            agent_ctx,
        )
        .await
    }

    /// Refuse a brand-new File BEFORE any of its bytes are stored: one that
    /// exists already, or whose target Workspace is not Active (`workspace_id`
    /// arrives in the create params: the File has no persisted state yet).
    pub(crate) async fn check_new_file_writable(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        workspace_id: &str,
    ) -> Result<(), FileStreamContentError> {
        if self.ensure_entity_loaded(tenant, "File", file_id).await {
            return Err(FileStreamContentError::ActionRejected(format!(
                "File('{file_id}') already exists; use File $value update for existing content"
            )));
        }
        self.reject_write_if_workspace_not_active(tenant, workspace_id)
            .await
    }

    /// Commit a brand-new File whose bytes are already stored under their
    /// content hash, as ONE atomic create-with-content append.
    pub(crate) async fn commit_new_file_content(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        create_params: serde_json::Value,
        content: &StoredFileContent,
        mime_type: &str,
        agent_ctx: &crate::request_context::AgentContext,
    ) -> Result<crate::entity_actor::EntityResponse, FileStreamContentError> {
        let Some((store, _backend)) = self.event_journal() else {
            return Err(FileStreamContentError::State(
                "File initial content create requires an event journal".to_string(),
            ));
        };
        let table = self.file_transition_table(tenant)?;
        let workspace_id = create_params
            .get("workspace_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();

        let persistence_id = format!("{tenant}:File:{file_id}");
        let mut state = initial_file_state(file_id, &table, serde_json::json!({}));
        let initial = state.clone();
        let mut events = Vec::with_capacity(3);

        let created = EntityEvent::bootstrap(state.status.clone());
        push_synthetic_event(&mut state, &mut events, created, agent_ctx);

        let no_xref = std::collections::BTreeMap::new();
        if create_params
            .as_object()
            .is_some_and(|params| !params.is_empty())
        {
            let create_event =
                apply_synthetic_file_action(&mut state, &table, "Create", create_params, &no_xref)?;
            push_synthetic_event(&mut state, &mut events, create_event, agent_ctx);
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
            "content_hash": content.content_hash,
            "size_bytes": content.size_bytes as i64,
            "mime_type": mime_type,
            "version_number": version_number,
            "previous_version_id": previous_version_id,
            "created_by": created_by,
        });
        // The `File.StreamUpdated` spec guard reads the owning Workspace's
        // status. On the live dispatch path that status is resolved by
        // `resolve_related_statuses`; this atomic synthetic path bypasses
        // that resolution, so supply it directly. We already confirmed the
        // workspace is Active (or absent) above.
        let mut stream_xref = temper_jit::table::RelatedMap::new();
        if !workspace_id.is_empty() {
            stream_xref.insert(
                ("Workspace".to_string(), "workspace_id".to_string()),
                temper_jit::table::Related::Statuses(vec![Some("Active".to_string())]),
            );
        }
        let stream_event = apply_synthetic_file_action(
            &mut state,
            &table,
            "StreamUpdated",
            stream_params,
            &stream_xref,
        )?;
        push_synthetic_event(&mut state, &mut events, stream_event, agent_ctx);

        let envelopes = events
            .iter()
            .enumerate()
            .map(|(idx, event)| {
                synthetic_envelope(&persistence_id, (idx + 1) as u64, event, &initial)
            })
            .collect::<Result<Vec<_>, _>>()?;

        // The record's query-plane row commits with its events, as a move's.
        let mut after = state.clone();
        after.sequence_nr = envelopes.len() as u64;
        let projection = store
            .commits_entity_rows()
            .then(|| self.append_projection(tenant, "File", &after));
        let rows = CommitRows {
            projection: projection.as_ref().map(AppendProjection::as_row),
            ..CommitRows::default()
        };
        match store
            .append_with_rows(&persistence_id, 0, &envelopes, rows)
            .await
        {
            Ok(sequence_nr) => state.sequence_nr = sequence_nr,
            Err(PersistenceError::ConcurrencyViolation { .. }) => {
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

        self.stop_and_remove_entity(tenant, "File", file_id);
        {
            let index_key = format!("{tenant}:File");
            let mut index = self.entity_index.write().unwrap();
            index
                .entry(index_key)
                .or_default()
                .insert(file_id.to_string());
        }

        let response = EntityResponse {
            success: true,
            refusal: None,
            state,
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
            refused: false,
            answered_action: None,
            trigger_failures: Vec::new(),
            store_unavailable: false,
            unrecorded_at: None,
        };
        self.broadcast_synthetic_file_stream_update(tenant, file_id, &response, agent_ctx);
        Ok(response)
    }

    pub(super) fn file_transition_table(
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
            let agent_ctx = agent_ctx.clone();
            // The reactions are part of the request that wrote the file: their span
            // is a child of the current one, as for every background reaction.
            let span = tracing::info_span!(
                "reaction.dispatch.background",
                tenant = %tenant,
                entity_type = "File",
                entity_id = %file_id,
                action_name = "StreamUpdated",
            );
            tokio::spawn(
                async move {
                    // determinism-ok: post-commit external reaction side effects
                    // determinism-ok: post-commit reaction side effects mirror the
                    // existing non-awaited File `$value` update behavior.
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
                }
                .instrument(span),
            );
        }
    }
}

fn initial_file_state(
    file_id: &str,
    table: &temper_jit::table::TransitionTable,
    initial_fields: serde_json::Value,
) -> EntityState {
    let fields = crate::entity_actor::effects::sanitize_action_params(&initial_fields).into_owned();
    crate::entity_actor::EntityActor::build_initial_state("File", file_id, table, &fields)
}

fn apply_synthetic_file_action(
    state: &mut EntityState,
    table: &temper_jit::table::TransitionTable,
    action: &str,
    params: serde_json::Value,
    related: &temper_jit::table::RelatedMap,
) -> Result<EntityEvent, FileStreamContentError> {
    let result = process_action_with_xref(state, table, action, &params, related);
    if !result.overflow_blobs.is_empty() {
        return Err(FileStreamContentError::State(format!(
            "File.{action} produced field-overflow blobs on the atomic initial content path"
        )));
    }
    if !result.success {
        return Err(FileStreamContentError::ActionRejected(
            result
                .error
                .unwrap_or_else(|| format!("File.{action} rejected")),
        ));
    }
    result.event.ok_or_else(|| {
        FileStreamContentError::State(format!(
            "File.{action} succeeded without producing a durable event"
        ))
    })
}

fn push_synthetic_event(
    state: &mut EntityState,
    events: &mut Vec<EntityEvent>,
    mut event: EntityEvent,
    caller: &crate::request_context::AgentContext,
) {
    event.origin = crate::entity_actor::EventOrigin::from_caller(Some(caller));
    state.sequence_nr = state.sequence_nr.saturating_add(1);
    state.push_event_bounded(event.clone());
    events.push(event);
}

fn synthetic_envelope(
    persistence_id: &str,
    sequence_nr: u64,
    event: &EntityEvent,
    initial: &EntityState,
) -> Result<PersistenceEnvelope, FileStreamContentError> {
    let payload = crate::entity_actor::bootstrap::event_payload(event, initial)
        .map_err(|e| FileStreamContentError::State(format!("failed to serialize event: {e}")))?;
    Ok(PersistenceEnvelope {
        sequence_nr,
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

#[cfg(test)]
mod default_tests {
    use super::*;

    #[test]
    fn round_four_atomic_file_initial_state_materializes_declared_defaults() {
        let table = temper_jit::table::TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "File"
states = ["Active"]
initial = "Active"
strict_action_params = true
[[state]]
name = "revision"
type = "counter"
initial = 3
[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Active"]
to = "Active"
params = ["expected"]
constraints = [{kind="param_equals_field",param="expected",field="revision"}]
"#,
        );
        let mut state = initial_file_state("file", &table, serde_json::json!({}));
        assert_eq!(state.counters.get("revision"), Some(&3));
        let result = apply_synthetic_file_action(
            &mut state,
            &table,
            "StreamUpdated",
            serde_json::json!({"expected":3}),
            &Default::default(),
        );
        assert!(
            result.is_ok(),
            "fresh File cannot use its declared initial state: {result:?}"
        );
    }
}
