use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};
use temper_wasm::{StreamRegistry, WasmInvocationContext};

use super::{DispatchCommand, DispatchExtOptions, ServerState};

struct FileStreamAction<'a> {
    tenant: &'a temper_runtime::tenant::TenantId,
    file_id: &'a str,
    action: &'a str,
    params: serde_json::Value,
    agent_ctx: &'a crate::request_context::AgentContext,
    await_reactions: bool,
    expected_authorization_precondition: Option<String>,
}

/// Error returned by the native/File `$value` content upload path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileStreamContentError {
    /// Failed to load or serialize entity state.
    State(String),
    /// Failed to write the content-addressed blob.
    BlobStore(String),
    /// Failed while preparing the legacy WASM fallback stream.
    StreamRegistry(String),
    /// The WASM blob adapter failed.
    Wasm(String),
    /// The verified File action rejected the content update.
    ActionRejected(String),
}

impl std::fmt::Display for FileStreamContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State(error)
            | Self::BlobStore(error)
            | Self::StreamRegistry(error)
            | Self::Wasm(error)
            | Self::ActionRejected(error) => f.write_str(error),
        }
    }
}

impl ServerState {
    /// Reject a File `$value` write when its owning Workspace is not `Active`.
    ///
    /// The `File.StreamUpdated` spec guard
    /// (`cross_entity_state` → `Workspace` in `["Active"]`) already aborts the
    /// transition for a `Frozen`/`Archived` workspace. But both File write
    /// paths persist the content blob BEFORE dispatching `StreamUpdated`, so a
    /// guard rejection alone would leave an orphaned blob with no committed
    /// state change. This pre-write check moves the rejection ahead of the
    /// byte write so a non-`Active` workspace is refused atomically, with no
    /// side effects.
    ///
    /// Vacuous when there is no `workspace_id` or the referenced Workspace
    /// cannot be resolved (e.g. standalone-File deployments / tests with no
    /// Workspace entity): there is no container whose freeze we can honour, so
    /// the write proceeds — matching the spec guard's empty-id semantics.
    pub(crate) async fn reject_write_if_workspace_not_active(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        workspace_id: &str,
    ) -> Result<(), FileStreamContentError> {
        if workspace_id.is_empty() {
            return Ok(());
        }
        match self
            .resolve_entity_status(tenant, "Workspace", workspace_id)
            .await
        {
            Some(status) if status != "Active" => {
                Err(FileStreamContentError::ActionRejected(format!(
                    "workspace '{workspace_id}' is '{status}', not 'Active'; \
                     it does not accept new file content"
                )))
            }
            // Active, or unresolvable (no Workspace entity) → allow.
            _ => Ok(()),
        }
    }

    /// Upload stream content for a TemperFS `File` entity, then dispatch the
    /// verified `StreamUpdated` action.
    ///
    /// This is the programmatic equivalent of `PUT /tdata/Files('{id}')/$value`
    /// and keeps platform-side bootstrapping aligned with normal stream writes.
    pub async fn put_file_stream_content(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        body: &[u8],
        mime_type: &str,
        agent_ctx: &crate::request_context::AgentContext,
    ) -> Result<crate::entity_actor::EntityResponse, String> {
        self.put_file_stream_content_checked(tenant, file_id, body, mime_type, agent_ctx, None)
            .await
            .map_err(|error| error.to_string())
    }

    /// Upload stream content for a TemperFS `File` entity and preserve typed
    /// failure classes for HTTP status mapping.
    #[tracing::instrument(skip_all, fields(
        otel.name = "state.put_file_stream_content",
        tenant = %tenant,
        file_id,
        request_bytes = body.len() as u64,
    ))]
    pub(crate) async fn put_file_stream_content_checked(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        body: &[u8],
        mime_type: &str,
        agent_ctx: &crate::request_context::AgentContext,
        expected_authorization_precondition: Option<String>,
    ) -> Result<crate::entity_actor::EntityResponse, FileStreamContentError> {
        let blob_endpoint = self
            .secrets_vault
            .as_ref()
            .and_then(|vault| vault.get_secret(&tenant.to_string(), "blob_endpoint"));
        let native_result = self
            .put_file_stream_content_native(
                tenant,
                file_id,
                body,
                mime_type,
                agent_ctx,
                expected_authorization_precondition.clone(),
            )
            .await;
        match native_result {
            Ok(response) => return Ok(response),
            Err(FileStreamContentError::BlobStore(error))
                if blob_endpoint.as_deref().is_some_and(|endpoint| {
                    !crate::blob_store::is_local_internal_blob_endpoint(endpoint)
                }) =>
            {
                tracing::warn!(
                    tenant = %tenant,
                    file_id,
                    error = %error,
                    "native File $value upload failed; falling back to blob_adapter"
                );
            }
            Err(error) => return Err(error),
        }

        self.put_file_stream_content_via_wasm(
            tenant,
            file_id,
            body,
            mime_type,
            agent_ctx,
            expected_authorization_precondition,
        )
        .await
    }

    #[tracing::instrument(skip_all, fields(
        otel.name = "state.put_file_stream_content.native",
        tenant = %tenant,
        file_id,
        request_bytes = body.len() as u64,
    ))]
    async fn put_file_stream_content_native(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        body: &[u8],
        mime_type: &str,
        agent_ctx: &crate::request_context::AgentContext,
        expected_authorization_precondition: Option<String>,
    ) -> Result<crate::entity_actor::EntityResponse, FileStreamContentError> {
        let (content_hash, blob_key) = content_hash_and_native_blob_key(body);

        // Load File state first so we can read its `workspace_id` and reject a
        // write into a non-Active workspace BEFORE persisting any bytes. The
        // blob write is intentionally NOT joined concurrently here: it must not
        // happen if the workspace refuses the write (otherwise an orphaned blob
        // would be left behind on guard rejection).
        let file_state = self
            .get_tenant_entity_state(tenant, "File", file_id)
            .await
            .map_err(|e| {
                FileStreamContentError::State(format!(
                    "failed to load File('{file_id}') state: {e}"
                ))
            })?;

        let workspace_id = file_state
            .state
            .fields
            .get("workspace_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        self.reject_write_if_workspace_not_active(tenant, workspace_id)
            .await?;
        if agent_ctx
            .expected_entity_sequence
            .is_some_and(|expected| expected != file_state.state.sequence_nr)
        {
            return Err(FileStreamContentError::ActionRejected(
                "SequenceConflict".into(),
            ));
        }

        self.put_content_addressed_blob(tenant, &blob_key, body, None)
            .await
            .map_err(|e| {
                FileStreamContentError::BlobStore(format!(
                    "failed to persist blob '{blob_key}': {e}"
                ))
            })?;

        let version_number = file_state
            .state
            .counters
            .get("version_count")
            .copied()
            .unwrap_or(0)
            + 1;
        let previous_version_id = file_state
            .state
            .fields
            .get("last_version_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let created_by = agent_ctx.agent_id.clone().unwrap_or_default();
        self.dispatch_file_stream_action(FileStreamAction {
            tenant,
            file_id,
            action: "StreamUpdated",
            params: serde_json::json!({
                "content_hash": content_hash,
                "size_bytes": body.len() as i64,
                "mime_type": mime_type,
                "version_number": version_number,
                "previous_version_id": previous_version_id,
                "created_by": created_by,
            }),
            agent_ctx,
            await_reactions: false,
            expected_authorization_precondition,
        })
        .await
        .map_err(|error| FileStreamContentError::ActionRejected(error.to_string()))
    }

    #[tracing::instrument(skip_all, fields(
        otel.name = "state.put_file_stream_content.wasm_fallback",
        tenant = %tenant,
        file_id,
        request_bytes = body.len() as u64,
    ))]
    async fn put_file_stream_content_via_wasm(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        body: &[u8],
        mime_type: &str,
        agent_ctx: &crate::request_context::AgentContext,
        expected_authorization_precondition: Option<String>,
    ) -> Result<crate::entity_actor::EntityResponse, FileStreamContentError> {
        let mut entity_state = serde_json::to_value(
            &self
                .get_tenant_entity_state(tenant, "File", file_id)
                .await
                .map_err(|e| {
                    FileStreamContentError::State(format!(
                        "failed to load File('{file_id}') state: {e}"
                    ))
                })?
                .state,
        )
        .map_err(|e| {
            FileStreamContentError::State(format!(
                "failed to serialize File('{file_id}') state: {e}"
            ))
        })?;
        crate::blobs::hydrate_blob_refs_for_tenant(self, tenant, &mut entity_state).await;

        let stream_id = format!("upload-{}", temper_runtime::scheduler::sim_uuid());
        let streams = Arc::new(RwLock::new(StreamRegistry::default()));
        {
            let mut registry = streams.write().map_err(|_| {
                FileStreamContentError::StreamRegistry(
                    "stream registry lock was poisoned".to_string(),
                )
            })?;
            registry.register_stream(&stream_id, body.to_vec());
        }

        let inv_ctx = WasmInvocationContext {
            tenant: tenant.to_string(),
            entity_type: "File".to_string(),
            entity_id: file_id.to_string(),
            trigger_action: "StreamUpload".to_string(),
            wasm_module: Some("blob_adapter".to_string()),
            trigger_params: serde_json::json!({
                "stream_id": stream_id,
                "size_bytes": body.len() as i64,
                "content_type": mime_type,
                "operation": "put",
            }),
            entity_state,
            agent_id: agent_ctx.agent_id.clone(),
            session_id: agent_ctx.session_id.clone(),
            integration_config: std::collections::BTreeMap::new(),
            trace_id: agent_ctx.trace_id.clone().unwrap_or_default(),
            workflow_root_entity_type: agent_ctx.workflow_root_entity_type.clone(),
            workflow_root_entity_id: agent_ctx.workflow_root_entity_id.clone(),
            workflow_run_id: agent_ctx.workflow_run_id.clone(),
            http_request: None,
        };

        let security_ctx = agent_ctx.security_ctx.as_ref().ok_or_else(|| {
            FileStreamContentError::Wasm(
                "blob_adapter requires the caller's authenticated security context".to_string(),
            )
        })?;
        let wasm_result = self
            .invoke_wasm_direct(tenant, "blob_adapter", inv_ctx, streams, security_ctx)
            .await
            .map_err(|e| FileStreamContentError::Wasm(format!("blob_adapter failed: {e}")))?;

        if !wasm_result.success {
            return Err(FileStreamContentError::Wasm(
                wasm_result
                    .error
                    .unwrap_or_else(|| "blob_adapter returned an unknown error".to_string()),
            ));
        }

        if wasm_result.callback_action.is_empty() {
            return self
                .get_tenant_entity_state(tenant, "File", file_id)
                .await
                .map_err(|e| {
                    FileStreamContentError::State(format!(
                        "failed to load File('{file_id}') state after blob_adapter: {e}"
                    ))
                });
        }

        self.dispatch_file_stream_action(FileStreamAction {
            tenant,
            file_id,
            action: &wasm_result.callback_action,
            params: wasm_result.callback_params,
            agent_ctx,
            await_reactions: true,
            expected_authorization_precondition,
        })
        .await
        .map_err(FileStreamContentError::ActionRejected)
    }

    async fn dispatch_file_stream_action(
        &self,
        request: FileStreamAction<'_>,
    ) -> Result<crate::entity_actor::EntityResponse, String> {
        let options = DispatchExtOptions {
            agent_ctx: request.agent_ctx,
            await_integration: false,
            await_reactions: request.await_reactions,
        };
        match request.expected_authorization_precondition {
            Some(expected) => self
                .dispatch_tenant_action_ext_typed_if_current(
                    DispatchCommand {
                        tenant: request.tenant,
                        entity_type: "File",
                        entity_id: request.file_id,
                        action: request.action,
                        params: request.params,
                        agent_ctx: options.agent_ctx,
                        await_integration: options.await_integration,
                        await_reactions: options.await_reactions,
                    },
                    expected,
                )
                .await
                .map_err(|error| error.to_string()),
            None => self
                .dispatch_tenant_action_ext_typed(
                    request.tenant,
                    "File",
                    request.file_id,
                    request.action,
                    request.params,
                    options,
                )
                .await
                .map_err(|error| error.to_string()),
        }
    }
}

pub(super) fn content_hash_and_native_blob_key(body: &[u8]) -> (String, String) {
    let mut hasher = Sha256::new();
    hasher.update(body);
    let content_hash = format!("sha256:{:x}", hasher.finalize());
    let blob_key = format!("temper-fs/{content_hash}");
    (content_hash, blob_key)
}
