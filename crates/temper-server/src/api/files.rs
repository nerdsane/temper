use std::collections::{BTreeMap, BTreeSet};

use axum::extract::{Extension, State};
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use tracing::instrument;

use crate::authz::{
    ResourceAuthorization, require_authenticated_context, require_resource_authorization,
};
use crate::state::{
    BatchTextReadError, PUBLISH_ARTIFACT_STALE_AUTHORIZATION, PublishArtifactAuthorization,
    PublishFileArtifactRequest, ServerState, validate_batch_text_ids,
};

#[derive(Debug, serde::Deserialize)]
pub(crate) struct BatchTextFileReadRequest {
    #[serde(default)]
    file_ids: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct BatchTextFileVersionReadRequest {
    #[serde(default)]
    file_version_ids: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct BatchTextFileReadResponse {
    files: Vec<crate::state::TextFileReadResult>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct BatchTextFileVersionReadResponse {
    files: Vec<crate::state::TextFileVersionReadResult>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct PublishArtifactRequest {
    file_id: String,
    label: String,
    #[serde(default)]
    owner_ref_type: String,
    #[serde(default)]
    owner_ref_id: String,
    #[serde(default)]
    source_file_version_id: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct PublishArtifactResponse {
    artifact: crate::storage::PublishedArtifactStoreRow,
}

async fn authoritative_file_resource_attrs(
    state: &ServerState,
    tenant: &temper_runtime::tenant::TenantId,
    resource_type: &str,
    resource_id: &str,
) -> Result<(BTreeMap<String, serde_json::Value>, Option<String>), axum::http::StatusCode> {
    let governed = state
        .has_registered_spec(tenant, resource_type)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    if !governed
        || !state
            .ensure_entity_loaded(tenant, resource_type, resource_id)
            .await
    {
        return Ok((BTreeMap::new(), None));
    }
    state
        .load_authz_resource_snapshot(tenant, resource_type, resource_id)
        .await
        .map(|snapshot| {
            let precondition = crate::entity_actor::effects::entity_authorization_precondition(
                &snapshot.current_state.state,
            );
            (snapshot.resource_attrs, Some(precondition))
        })
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}

async fn require_file_resources(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    resource_type: &str,
    resource_ids: &[String],
) -> Result<(), axum::http::StatusCode> {
    let mut checked = BTreeSet::new();
    for resource_id in resource_ids {
        if checked.insert(resource_id.as_str()) {
            let (resource_attrs, _) = authoritative_file_resource_attrs(
                state,
                authenticated.tenant(),
                resource_type,
                resource_id,
            )
            .await?;
            require_resource_authorization(
                state,
                authenticated,
                ResourceAuthorization {
                    action: "read",
                    resource_type,
                    resource_id,
                    resource_attrs,
                },
            )?;
        }
    }
    Ok(())
}

fn batch_read_error_response(error: BatchTextReadError) -> axum::response::Response {
    let (status, code) = match &error {
        BatchTextReadError::InvalidRequest(_) => {
            (axum::http::StatusCode::BAD_REQUEST, "InvalidBatchFileRead")
        }
        BatchTextReadError::TooManyItems { .. }
        | BatchTextReadError::ItemTooLarge { .. }
        | BatchTextReadError::ResponseTooLarge { .. } => (
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "BatchFileReadBudgetExceeded",
        ),
        BatchTextReadError::Storage(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "BatchFileReadFailed",
        ),
    };
    (
        status,
        axum::Json(serde_json::json!({
            "error": { "code": code, "message": error.to_string() }
        })),
    )
        .into_response()
}

/// POST /api/files/read-text-batch — read many text file bodies via the
/// projection-backed immutable content path.
#[instrument(skip_all, fields(otel.name = "POST /api/files/read-text-batch"))]
pub(crate) async fn handle_read_text_batch(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(body): axum::Json<BatchTextFileReadRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    let tenant = authenticated.tenant();
    if let Err(error) = validate_batch_text_ids(&body.file_ids) {
        return batch_read_error_response(error);
    }
    if let Err(status) = require_file_resources(&state, authenticated, "File", &body.file_ids).await
    {
        return status.into_response();
    }

    match state.read_file_texts_batch(tenant, &body.file_ids).await {
        Ok(files) => (
            axum::http::StatusCode::OK,
            axum::Json(BatchTextFileReadResponse { files }),
        )
            .into_response(),
        Err(error) => batch_read_error_response(error),
    }
}

/// POST /api/files/read-version-text-batch — read many immutable file version
/// bodies via projections + direct blob fetch.
#[instrument(skip_all, fields(otel.name = "POST /api/files/read-version-text-batch"))]
pub(crate) async fn handle_read_version_text_batch(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(body): axum::Json<BatchTextFileVersionReadRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    let tenant = authenticated.tenant();
    if let Err(error) = validate_batch_text_ids(&body.file_version_ids) {
        return batch_read_error_response(error);
    }
    if let Err(status) =
        require_file_resources(&state, authenticated, "FileVersion", &body.file_version_ids).await
    {
        return status.into_response();
    }

    match state
        .read_file_version_texts_batch(tenant, &body.file_version_ids)
        .await
    {
        Ok(files) => (
            axum::http::StatusCode::OK,
            axum::Json(BatchTextFileVersionReadResponse { files }),
        )
            .into_response(),
        Err(error) => batch_read_error_response(error),
    }
}

/// POST /api/files/publish-artifact — promote a governed TemperFS file into
/// an immutable public blob and persist the public artifact record.
#[instrument(skip_all, fields(
    otel.name = "POST /api/files/publish-artifact",
    tenant = tracing::field::Empty,
    file_id = tracing::field::Empty,
    source_file_version_id = tracing::field::Empty,
    artifact_label = tracing::field::Empty,
    owner_ref_type = tracing::field::Empty,
    owner_ref_id = tracing::field::Empty,
    artifact_id = tracing::field::Empty,
    public_storage_key = tracing::field::Empty,
    public_url = tracing::field::Empty,
    byte_length = tracing::field::Empty,
    artifact_status = tracing::field::Empty,
))]
pub(crate) async fn handle_publish_artifact(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(body): axum::Json<PublishArtifactRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    let tenant = authenticated.tenant();
    let span = tracing::Span::current();
    span.record("tenant", tenant.as_str());
    span.record("file_id", body.file_id.as_str());
    span.record(
        "source_file_version_id",
        body.source_file_version_id.as_str(),
    );
    span.record("artifact_label", body.label.as_str());
    span.record("owner_ref_type", body.owner_ref_type.as_str());
    span.record("owner_ref_id", body.owner_ref_id.as_str());

    if body.file_id.trim().is_empty() || body.label.trim().is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "InvalidPublishArtifactRequest",
                    "message": "file_id and label are required",
                }
            })),
        )
            .into_response();
    }

    let request_attrs = BTreeMap::from([
        (
            "requestedFileId".to_string(),
            serde_json::Value::String(body.file_id.clone()),
        ),
        (
            "requestedSourceFileVersionId".to_string(),
            serde_json::Value::String(body.source_file_version_id.clone()),
        ),
        (
            "requestedLabel".to_string(),
            serde_json::Value::String(body.label.clone()),
        ),
        (
            "requestedOwnerRefType".to_string(),
            serde_json::Value::String(body.owner_ref_type.clone()),
        ),
        (
            "requestedOwnerRefId".to_string(),
            serde_json::Value::String(body.owner_ref_id.clone()),
        ),
        (
            "requestedNamespace".to_string(),
            serde_json::Value::String(body.namespace.clone().unwrap_or_default()),
        ),
    ]);
    let (mut file_resource_attrs, file_state_precondition) =
        match authoritative_file_resource_attrs(&state, tenant, "File", &body.file_id).await {
            Ok(attrs) => attrs,
            Err(status) => return status.into_response(),
        };
    let file_authorization =
        file_state_precondition.map(|state_precondition| PublishArtifactAuthorization {
            source_entity_type: "File".to_string(),
            source_entity_id: body.file_id.clone(),
            state_precondition,
            resource_attrs: file_resource_attrs.clone(),
        });
    file_resource_attrs.extend(request_attrs.clone());
    let publish_authorizations = if body.source_file_version_id.trim().is_empty() {
        if let Err(status) = require_resource_authorization(
            &state,
            authenticated,
            ResourceAuthorization {
                action: "publish_artifact",
                resource_type: "File",
                resource_id: &body.file_id,
                resource_attrs: file_resource_attrs,
            },
        ) {
            return status.into_response();
        }
        file_authorization.into_iter().collect()
    } else {
        let (mut version_resource_attrs, version_state_precondition) =
            match authoritative_file_resource_attrs(
                &state,
                tenant,
                "FileVersion",
                &body.source_file_version_id,
            )
            .await
            {
                Ok(attrs) => attrs,
                Err(status) => return status.into_response(),
            };
        let authoritative_source_file_id = version_resource_attrs
            .get("file_id")
            .and_then(serde_json::Value::as_str)
            .filter(|file_id| !file_id.is_empty())
            .map(str::to_string);
        let version_authorization =
            version_state_precondition.map(|state_precondition| PublishArtifactAuthorization {
                source_entity_type: "FileVersion".to_string(),
                source_entity_id: body.source_file_version_id.clone(),
                state_precondition,
                resource_attrs: version_resource_attrs.clone(),
            });
        version_resource_attrs.extend(request_attrs);
        if let Err(status) = require_resource_authorization(
            &state,
            authenticated,
            ResourceAuthorization {
                action: "publish_artifact",
                resource_type: "FileVersion",
                resource_id: &body.source_file_version_id,
                resource_attrs: version_resource_attrs,
            },
        ) {
            return status.into_response();
        }
        let stored_file_id = if let Some(file_id) = authoritative_source_file_id {
            file_id
        } else {
            match state
                .file_version_source_file_id(tenant, &body.source_file_version_id)
                .await
            {
                Ok(file_id) => file_id,
                Err(error) => {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({
                            "error": {
                                "code": "FileVersionRelationshipReadFailed",
                                "message": error,
                            }
                        })),
                    )
                        .into_response();
                }
            }
        };
        if stored_file_id != body.file_id {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": {
                        "code": "FileVersionFileMismatch",
                        "message": format!(
                            "FileVersion '{}' belongs to File '{}', not '{}'",
                            body.source_file_version_id, stored_file_id, body.file_id,
                        ),
                    }
                })),
            )
                .into_response();
        }
        if let Err(status) = require_resource_authorization(
            &state,
            authenticated,
            ResourceAuthorization {
                action: "publish_artifact",
                resource_type: "File",
                resource_id: &body.file_id,
                resource_attrs: file_resource_attrs,
            },
        ) {
            return status.into_response();
        }
        file_authorization
            .into_iter()
            .chain(version_authorization)
            .collect()
    };

    let publish_request = PublishFileArtifactRequest {
        file_id: body.file_id,
        label: body.label,
        owner_ref_type: body.owner_ref_type,
        owner_ref_id: body.owner_ref_id,
        source_file_version_id: body.source_file_version_id,
        namespace: body.namespace,
    };
    if let Err(error) = publish_request.validate() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "InvalidPublishArtifactRequest",
                    "message": error,
                }
            })),
        )
            .into_response();
    }

    match state
        .publish_file_artifact_authorized(tenant, publish_request, publish_authorizations)
        .await
    {
        Ok(artifact) => {
            span.record("artifact_id", artifact.id.as_str());
            span.record("public_storage_key", artifact.public_storage_key.as_str());
            span.record("public_url", artifact.public_url.as_str());
            span.record("byte_length", artifact.byte_length);
            span.record("artifact_status", artifact.status.as_str());
            tracing::info!(
                tenant = tenant.as_str(),
                file_id = artifact.source_file_id.as_str(),
                source_file_version_id = artifact.source_file_version_id.as_str(),
                artifact_id = artifact.id.as_str(),
                artifact_label = artifact.label.as_str(),
                public_storage_key = artifact.public_storage_key.as_str(),
                public_url = artifact.public_url.as_str(),
                byte_length = artifact.byte_length,
                artifact_status = artifact.status.as_str(),
                owner_ref_type = artifact.owner_ref_type.as_str(),
                owner_ref_id = artifact.owner_ref_id.as_str(),
                "publish artifact request completed"
            );
            (
                axum::http::StatusCode::OK,
                axum::Json(PublishArtifactResponse { artifact }),
            )
                .into_response()
        }
        Err(error) => {
            let (status, code) = if error == PUBLISH_ARTIFACT_STALE_AUTHORIZATION {
                (axum::http::StatusCode::CONFLICT, "ConcurrentModification")
            } else {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "PublishArtifactFailed",
                )
            };
            (
                status,
                axum::Json(serde_json::json!({
                    "error": { "code": code, "message": error }
                })),
            )
                .into_response()
        }
    }
}
