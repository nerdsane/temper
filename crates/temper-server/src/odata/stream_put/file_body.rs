//! `PUT Files('<id>')/$value`, streamed: the body is never held in memory and is not bound by the server's buffered
//! body limit. Cedar decides first, from the headers alone; then the body is staged on disk while it is hashed,
//! sent on to the tenant's blob store under its sha256, and only then is the File's new content committed.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use futures_util::TryStreamExt as _;
use temper_authz::SecurityContext;
use temper_odata::path::ODataPath;
use temper_runtime::tenant::TenantId;

use super::super::authz::{MutationResource, UPDATE_ACTION, authorize_mutation};
use super::super::common::{
    authorized_snapshot_failure, check_has_stream_or_400, resolve_entity_type, resolve_value_parent,
};
use super::super::content_addressed::responses::{blob_store_error_response, stage_error_response};
use super::{
    check_verification_gate_or_423, file_stream_content_error_response, stream_put_success_response,
};
use crate::blob_store::{BlobByteStream, BlobIngestAdmissionError, MAX_RAW_BLOB_BYTES};
use crate::request_context::AgentContext;
use crate::response::odata_error;
use crate::state::redecide::Redecide;
use crate::state::{ServerState, StoredFileContent, native_file_blob_key};

/// The File a `$value` PUT names, when the server streams it: the TemperFS `File` type, a media entity, sent with
/// its `Content-Length`. A body sent without one is buffered as before, within the router's body limit.
pub(in crate::odata) fn streamed_file_target(
    state: &ServerState,
    tenant: &TenantId,
    path: &ODataPath,
    headers: &HeaderMap,
) -> Option<String> {
    let ODataPath::Value { parent } = path else {
        return None;
    };
    headers.get("content-length")?;
    let (set_name, key) = resolve_value_parent(parent).ok()?;
    let entity_type = resolve_entity_type(state, tenant, &set_name)?;
    // A local store keeps a copy of the bytes in the database, which the buffered path writes; stream only to a
    // remote object store, which keeps them alone.
    let remote = state
        .blob_store_for_tenant(tenant)
        .is_ok_and(|store| store.is_object_store());
    (remote
        && entity_type == "File"
        && check_has_stream_or_400(state, tenant, &entity_type).is_ok())
    .then_some(key)
}

/// Upload a File's bytes from the request body as it arrives.
#[tracing::instrument(skip_all, fields(otel.name = "PUT $value (streamed)", tenant = %tenant, file_id))]
pub(in crate::odata) async fn handle_file_value_put(
    state: &ServerState,
    tenant: &TenantId,
    file_id: &str,
    headers: &HeaderMap,
    body: Body,
    agent_ctx: &AgentContext,
    security_ctx: &SecurityContext,
) -> axum::response::Response {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let Some(declared_len) = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
    else {
        return odata_error(
            StatusCode::LENGTH_REQUIRED,
            "MissingContentLength",
            "A File's bytes are sent with a Content-Length header",
        )
        .into_response();
    };

    // Decided before a byte of the body is read, so nothing is stored for a
    // write that would be refused.
    let exists = state.ensure_entity_loaded(tenant, "File", file_id).await;
    let precondition =
        match decide_file_write(state, tenant, file_id, exists, agent_ctx, security_ctx).await {
            Ok(precondition) => precondition,
            Err(response) => return *response,
        };

    let mut admission = match state.file_ingest_budget.try_reserve(tenant, declared_len) {
        Ok(permit) => permit,
        Err(BlobIngestAdmissionError::ObjectTooLarge) => {
            return odata_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "ObjectTooLarge",
                &format!(
                    "A File is at most {MAX_RAW_BLOB_BYTES} bytes; this one declares {declared_len}"
                ),
            )
            .into_response();
        }
        Err(BlobIngestAdmissionError::BudgetExhausted | BlobIngestAdmissionError::TenantBusy) => {
            return odata_error(
                StatusCode::TOO_MANY_REQUESTS,
                "FileUploadsBusy",
                "Too many File uploads are under way here; try again in a moment",
            )
            .into_response();
        }
    };
    let store = match state.blob_store_for_tenant(tenant) {
        Ok(store) => store,
        Err(error) => {
            return odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "BlobStoreUnavailable",
                &error,
            )
            .into_response();
        }
    };
    let stream: BlobByteStream = Box::pin(
        body.into_data_stream()
            .map_err(|error| std::io::Error::other(error.to_string())),
    );
    let staged = match store
        .stage_file_stream(
            stream,
            declared_len,
            state.file_ingest_budget.progress_policy(),
            &mut admission,
        )
        .await
    {
        Ok(staged) => staged,
        Err(error) => return stage_error_response(error),
    };
    let content = StoredFileContent {
        content_hash: staged.content_hash(),
        size_bytes: staged.len() as u64,
    };
    if let Err(error) = store
        .put_staged_file(&native_file_blob_key(&content.content_hash), &staged)
        .await
    {
        return blob_store_error_response(&error);
    }
    drop(staged);
    drop(admission);

    let committed =
        if exists {
            // The bytes are stored by now, so a File that changed while they
            // streamed is decided again and its content committed again, without
            // reading a byte twice. The deadline runs from here, not from the
            // request: the upload was not a wait on the record.
            let mut redecide = Redecide::start(state);
            let mut decided_on = precondition;
            loop {
                redecide.dispatching().await;
                let answer = state
                    .commit_file_stream_content(
                        tenant,
                        file_id,
                        &content,
                        &content_type,
                        agent_ctx,
                        decided_on,
                    )
                    .await;
                let stale = matches!(&answer, Ok(entity_resp) if entity_resp.is_record_changed());
                if !redecide.again(stale, "file_value_put") {
                    break answer;
                }
                decided_on =
                    match decide_file_write(state, tenant, file_id, true, agent_ctx, security_ctx)
                        .await
                    {
                        Ok(decided) => decided,
                        Err(response) => return *response,
                    };
            }
        } else {
            state
                .commit_new_file_content(
                    tenant,
                    file_id,
                    serde_json::json!({}),
                    &content,
                    &content_type,
                    agent_ctx,
                )
                .await
        };
    match committed {
        Ok(entity_resp) => stream_put_success_response(&entity_resp),
        Err(error) => file_stream_content_error_response(error),
    }
}

/// Cedar's `update` on the File, as the buffered `$value` PUT decides it: for a new File on the attributes it would
/// be created with, for an existing one on its current state, whose authorization precondition the commit then holds.
/// Every check a File's content write is decided on: who may write it, and
/// whether the File can take content now (an existing one written once, a
/// frozen workspace). The first decision and each one again on a changed File
/// run this one function, so a check added here is decided again with the rest.
async fn decide_file_write(
    state: &ServerState,
    tenant: &TenantId,
    file_id: &str,
    exists: bool,
    agent_ctx: &AgentContext,
    security_ctx: &SecurityContext,
) -> Result<Option<String>, Box<axum::response::Response>> {
    let precondition =
        authorize_file_write(state, tenant, file_id, exists, agent_ctx, security_ctx).await?;
    let writable = if exists {
        state
            .check_existing_file_writable(tenant, file_id, true)
            .await
    } else {
        state.check_new_file_writable(tenant, file_id, "").await
    };
    writable.map_err(|error| Box::new(file_stream_content_error_response(error)))?;
    Ok(precondition)
}

async fn authorize_file_write(
    state: &ServerState,
    tenant: &TenantId,
    file_id: &str,
    exists: bool,
    agent_ctx: &AgentContext,
    security_ctx: &SecurityContext,
) -> Result<Option<String>, Box<axum::response::Response>> {
    if !exists {
        let attrs = state
            .synthesize_new_file_resource_attrs(tenant, file_id)
            .map_err(|error| {
                Box::new(
                    odata_error(StatusCode::INTERNAL_SERVER_ERROR, "StateError", &error)
                        .into_response(),
                )
            })?;
        authorize_mutation(
            state,
            tenant,
            security_ctx,
            agent_ctx,
            UPDATE_ACTION,
            MutationResource {
                entity_type: "File",
                entity_id: file_id,
                attrs: &attrs,
            },
        )
        .await
        .map_err(Box::new)?;
        check_verification_gate_or_423(state, tenant, "File")?;
        return Ok(None);
    }
    let snapshot = state
        .load_authz_resource_snapshot(tenant, "File", file_id)
        .await
        .map_err(|error| {
            Box::new(
                odata_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "ReadError",
                    &error.to_string(),
                )
                .into_response(),
            )
        })?;
    authorize_mutation(
        state,
        tenant,
        security_ctx,
        agent_ctx,
        UPDATE_ACTION,
        MutationResource {
            entity_type: "File",
            entity_id: file_id,
            attrs: &snapshot.resource_attrs,
        },
    )
    .await
    .map_err(Box::new)?;
    check_verification_gate_or_423(state, tenant, "File")?;
    let snapshot = state
        .materialize_authorized_snapshot(tenant, "File", file_id, snapshot)
        .await
        .map_err(|error| Box::new(authorized_snapshot_failure(&error).into_response()))?;
    Ok(Some(
        crate::entity_actor::effects::entity_authorization_precondition(
            &snapshot.current_state.state,
        ),
    ))
}
