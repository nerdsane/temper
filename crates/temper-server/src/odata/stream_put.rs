//! OData `$value` upload handlers.

use std::sync::{Arc, RwLock};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use temper_authz::SecurityContext;
use temper_odata::path::ODataPath;
use temper_runtime::tenant::TenantId;
use temper_wasm::{StreamRegistry, WasmInvocationContext};
use tracing::instrument;

use super::authz::{MutationResource, UPDATE_ACTION, authorize_mutation};
use super::common::{
    check_has_stream_or_400, resolve_entity_type, resolve_value_parent, verification_gate_response,
};
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::response::odata_error;
use crate::state::{FileStreamContentError, ServerState};

type ODataStreamPutError = Box<axum::response::Response>;

fn resolve_entity_type_or_404(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
) -> Result<String, ODataStreamPutError> {
    resolve_entity_type(state, tenant, set_name).ok_or_else(|| {
        tracing::warn!(tenant = %tenant, entity_set = %set_name, "entity set not found");
        Box::new(
            odata_error(
                StatusCode::NOT_FOUND,
                "EntitySetNotFound",
                &format!("Entity set '{set_name}' not found"),
            )
            .into_response(),
        )
    })
}

fn check_verification_gate_or_423(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> Result<(), ODataStreamPutError> {
    state
        .check_verification_gate(tenant, entity_type)
        .map_err(|e| Box::new(verification_gate_response(e)))
}

/// Handle PUT on `$value` — upload binary content via native File fast path or
/// the generic WASM blob adapter.
///
/// Flow:
/// 1. Resolve parent entity from ODataPath
/// 2. Verify entity type has `HasStream=true` in CSDL
/// 3. Use the native built-in File path when eligible
/// 4. Otherwise register upload bytes in StreamRegistry
/// 5. Invoke WASM blob_adapter
/// 6. Dispatch whatever action WASM returns (e.g. StreamUpdated)
/// 7. Return 204 No Content with ETag
#[instrument(skip_all, fields(otel.name = "PUT $value"))]
pub(super) async fn handle_stream_put(
    state: &ServerState,
    tenant: &TenantId,
    parent: &ODataPath,
    headers: &HeaderMap,
    body: axum::body::Bytes,
    agent_ctx: &AgentContext,
    security_ctx: &SecurityContext,
) -> axum::response::Response {
    let (set_name, key) = match resolve_value_parent(parent) {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    let entity_type = match resolve_entity_type_or_404(state, tenant, &set_name) {
        Ok(t) => t,
        Err(resp) => return *resp,
    };

    if let Err(resp) = check_has_stream_or_400(state, tenant, &entity_type) {
        return resp;
    }

    if let Err(resp) = check_verification_gate_or_423(state, tenant, &entity_type) {
        return *resp;
    }

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // ARN-87 write-side: a brand-new File's first `$value` write must commit as
    // ONE atomic create-with-content append. The default path below would
    // instead spawn the actor (whose `pre_start` persists an empty bootstrap
    // `Created`) and only then dispatch `StreamUpdated` as a second append —
    // leaving the File durable and projection-visible with an empty `$value`
    // between the two. The existence probe uses `ensure_entity_loaded`, which
    // for a truly-new id returns false WITHOUT spawning, so it does not itself
    // open that window; existing Files fall through to the unchanged path. The
    // File spec has no Delete/tombstone action (states: Created/Ready/Locked/
    // Archived), so a false probe always means a genuinely-new id — never a
    // deleted one; an existing Archived File probes true and takes the
    // unchanged path (where ArchivedIsFinal rejects the write).
    if entity_type == "File" && !state.ensure_entity_loaded(tenant, "File", &key).await {
        let new_file_attrs = match state.synthesize_new_file_resource_attrs(tenant, &key) {
            Ok(attrs) => attrs,
            Err(error) => {
                return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "StateError", &error)
                    .into_response();
            }
        };
        if let Err(response) = authorize_mutation(
            state,
            tenant,
            security_ctx,
            agent_ctx,
            UPDATE_ACTION,
            MutationResource {
                entity_type: &entity_type,
                entity_id: &key,
                attrs: &new_file_attrs,
            },
        )
        .await
        {
            return response;
        }
        return match state
            .create_file_with_initial_stream_content_checked(
                tenant,
                &key,
                serde_json::json!({}),
                body.as_ref(),
                &content_type,
                agent_ctx,
            )
            .await
        {
            Ok(entity_resp) => stream_put_success_response(&entity_resp),
            Err(error) => file_stream_content_error_response(error),
        };
    }

    let snapshot = match state
        .load_authz_resource_snapshot(tenant, &entity_type, &key)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return odata_error(StatusCode::NOT_FOUND, "ResourceNotFound", &error).into_response();
        }
    };
    if let Err(response) = authorize_mutation(
        state,
        tenant,
        security_ctx,
        agent_ctx,
        UPDATE_ACTION,
        MutationResource {
            entity_type: &entity_type,
            entity_id: &key,
            attrs: &snapshot.resource_attrs,
        },
    )
    .await
    {
        return response;
    }

    if entity_type == "File" {
        return match state
            .put_file_stream_content_checked(tenant, &key, body.as_ref(), &content_type, agent_ctx)
            .await
        {
            Ok(entity_resp) => stream_put_success_response(&entity_resp),
            Err(error) => file_stream_content_error_response(error),
        };
    }

    let entity_state = match state
        .get_tenant_entity_state(tenant, &entity_type, &key)
        .await
    {
        Ok(resp) => {
            let mut entity_state = serde_json::to_value(&resp.state).unwrap_or_default();
            hydrate_blob_refs_for_tenant(state, tenant, &mut entity_state).await;
            entity_state
        }
        Err(e) => {
            return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "StateError", &e)
                .into_response();
        }
    };

    let size_bytes = body.len() as i64;
    let stream_id = format!("upload-{}", temper_runtime::scheduler::sim_uuid());
    let streams = Arc::new(RwLock::new(StreamRegistry::default()));
    {
        let mut s = match streams.write() {
            Ok(lock) => lock,
            Err(_) => {
                return odata_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "StreamRegistryPoisoned",
                    "stream registry lock was poisoned",
                )
                .into_response();
            }
        };
        s.register_stream(&stream_id, body.to_vec());
    }

    let inv_ctx = WasmInvocationContext {
        tenant: tenant.to_string(),
        entity_type: entity_type.clone(),
        entity_id: key.clone(),
        trigger_action: "StreamUpload".to_string(),
        wasm_module: Some("blob_adapter".to_string()),
        trigger_params: serde_json::json!({
            "stream_id": stream_id,
            "size_bytes": size_bytes,
            "content_type": content_type,
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

    let wasm_result = match state
        .invoke_wasm_direct(tenant, "blob_adapter", inv_ctx, streams)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "WASM blob_adapter invocation failed");
            return odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BlobAdapterError",
                &format!("Blob adapter failed: {e}"),
            )
            .into_response();
        }
    };

    if !wasm_result.success {
        let error_msg = wasm_result
            .error
            .unwrap_or_else(|| "unknown error".to_string());
        return odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BlobUploadFailed",
            &error_msg,
        )
        .into_response();
    }

    if !wasm_result.callback_action.is_empty() {
        match state
            .dispatch_tenant_action(
                tenant,
                &entity_type,
                &key,
                &wasm_result.callback_action,
                wasm_result.callback_params,
                agent_ctx,
            )
            .await
        {
            Ok(entity_resp) => stream_put_success_response(&entity_resp),
            Err(e) => odata_error(StatusCode::CONFLICT, "ActionRejected", &e).into_response(),
        }
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}

fn stream_put_success_response(entity_resp: &EntityResponse) -> axum::response::Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert("OData-Version", HeaderValue::from_static("4.0"));
    if let Some(hash) = entity_response_content_hash(entity_resp)
        && let Ok(val) = format!("\"{hash}\"").parse()
    {
        response.headers_mut().insert("ETag", val);
    }
    response
}

fn entity_response_content_hash(entity_resp: &EntityResponse) -> Option<&str> {
    entity_resp
        .state
        .fields
        .get("content_hash")
        .and_then(|value| value.as_str())
}

fn file_stream_content_error_response(error: FileStreamContentError) -> axum::response::Response {
    match error {
        FileStreamContentError::ActionRejected(error) => {
            odata_error(StatusCode::CONFLICT, "ActionRejected", &error).into_response()
        }
        FileStreamContentError::State(error) => {
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "StateError", &error).into_response()
        }
        FileStreamContentError::BlobStore(error) => {
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "BlobStoreError", &error).into_response()
        }
        FileStreamContentError::StreamRegistry(error) => odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "StreamRegistryPoisoned",
            &error,
        )
        .into_response(),
        FileStreamContentError::Wasm(error) => odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BlobAdapterError",
            &error,
        )
        .into_response(),
    }
}
