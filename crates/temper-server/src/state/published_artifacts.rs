use reqwest::header::{CONTENT_TYPE, HeaderMap};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use temper_runtime::tenant::TenantId;
use tracing::{Span, instrument};

use crate::aws_sigv4::{self, parse_url_host_path, sha256_hex};
use crate::storage::{PublishedArtifactStoreRow, PublishedArtifactStoreUpsert};

use super::{IndexedFileStreamRead, ServerState};
use telemetry::{PublishedArtifactTelemetry, emit_published_artifact_persisted_log};

mod telemetry;

const DEFAULT_PUBLIC_ARTIFACT_NAMESPACE: &str = "published-artifacts";
pub(crate) const PUBLISH_ARTIFACT_STALE_AUTHORIZATION: &str =
    "publish source authorization became stale; retry against current state";

/// Exact source state/resource view used for a public-artifact Cedar decision.
pub(crate) struct PublishArtifactAuthorization {
    pub source_entity_type: String,
    pub source_entity_id: String,
    pub state_precondition: String,
    pub resource_attrs: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct PublishFileArtifactRequest {
    pub file_id: String,
    pub label: String,
    pub owner_ref_type: String,
    pub owner_ref_id: String,
    pub source_file_version_id: String,
    pub namespace: Option<String>,
}

impl PublishFileArtifactRequest {
    /// Validate every caller-controlled object-key component as one segment.
    pub fn validate(&self) -> Result<(), String> {
        validate_path_segment("label", &self.label)?;
        validate_path_segment("owner_ref_type", &self.owner_ref_type)?;
        validate_path_segment("owner_ref_id", &self.owner_ref_id)?;
        if let Some(namespace) = self.namespace.as_deref() {
            validate_path_segment("namespace", namespace)?;
        }
        Ok(())
    }
}

impl ServerState {
    /// Resolve the immutable parent File recorded by a FileVersion.
    ///
    /// The durable query projection is the fast path. Actor state is the
    /// read-after-write fallback when the projection has not caught up yet.
    #[cfg(feature = "observe")]
    pub(crate) async fn file_version_source_file_id(
        &self,
        tenant: &TenantId,
        file_version_id: &str,
    ) -> Result<String, String> {
        if let Some(query_plane) = self.query_plane_store() {
            let ids = [file_version_id.to_string()];
            let rows = query_plane
                .load_projection_fields_many(tenant.as_str(), "FileVersion", &ids, &["file_id"])
                .await
                .map_err(|error| {
                    format!("failed to load FileVersion '{file_version_id}' relationship: {error}")
                })?
                .unwrap_or_default();
            if let Some(file_id) = rows
                .first()
                .and_then(|row| row.fields.get("file_id"))
                .and_then(Option::as_deref)
                .filter(|file_id| !file_id.is_empty())
            {
                return Ok(file_id.to_string());
            }
        }

        let response = self
            .get_tenant_entity_state(tenant, "FileVersion", file_version_id)
            .await
            .map_err(|error| {
                format!("failed to load FileVersion '{file_version_id}' relationship: {error}")
            })?;
        response
            .state
            .fields
            .get("file_id")
            .and_then(serde_json::Value::as_str)
            .filter(|file_id| !file_id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                format!("FileVersion '{file_version_id}' has no immutable file_id relationship")
            })
    }

    #[instrument(skip_all, fields(
        otel.name = "state.publish_file_artifact",
        tenant = %tenant,
        file_id = %request.file_id,
        source_file_id = %request.file_id,
        source_file_version_id = %request.source_file_version_id,
        artifact_label = %request.label,
        owner_ref_type = %request.owner_ref_type,
        owner_ref_id = %request.owner_ref_id,
        artifact_id = tracing::field::Empty,
        content_hash = tracing::field::Empty,
        mime_type = tracing::field::Empty,
        byte_length = tracing::field::Empty,
        public_storage_key = tracing::field::Empty,
        public_url = tracing::field::Empty,
        artifact_status = tracing::field::Empty,
        metadata_backend = tracing::field::Empty,
        artifact_namespace = tracing::field::Empty,
        public_blob_bucket = tracing::field::Empty,
        public_blob_endpoint_host = tracing::field::Empty,
    ))]
    pub async fn publish_file_artifact(
        &self,
        tenant: &TenantId,
        request: PublishFileArtifactRequest,
    ) -> Result<PublishedArtifactStoreRow, String> {
        self.publish_file_artifact_authorized(tenant, request, Vec::new())
            .await
    }

    /// Publish only while the source still matches the exact Cedar-authorized
    /// state and derived resource attributes.
    pub(crate) async fn publish_file_artifact_authorized(
        &self,
        tenant: &TenantId,
        request: PublishFileArtifactRequest,
        authorizations: Vec<PublishArtifactAuthorization>,
    ) -> Result<PublishedArtifactStoreRow, String> {
        request.validate()?;
        let source_display = if request.source_file_version_id.trim().is_empty() {
            format!("File('{}')", request.file_id)
        } else {
            format!("FileVersion('{}')", request.source_file_version_id)
        };
        let stream = if request.source_file_version_id.trim().is_empty() {
            self.read_file_stream_indexed(tenant, &request.file_id)
                .await?
        } else {
            self.read_file_version_stream_indexed(tenant, &request.source_file_version_id)
                .await?
        };
        let (content_hash, mime_type, bytes) = match stream {
            IndexedFileStreamRead::Content {
                content_hash,
                mime_type,
                bytes,
            } => (content_hash, mime_type, bytes),
            IndexedFileStreamRead::NoContent { .. } => {
                return Err(format!("{source_display} has no content to publish"));
            }
            IndexedFileStreamRead::MissingIndex => {
                return Err(format!(
                    "{source_display} is missing from the file read index; rebuild projections before publishing",
                ));
            }
            IndexedFileStreamRead::StaleIndex { content_hash, .. } => {
                return Err(format!(
                    "{source_display} has stale indexed content '{content_hash}'; rebuild projections before publishing",
                ));
            }
        };

        for authorization in authorizations {
            let snapshot = self
                .load_authz_resource_snapshot(
                    tenant,
                    &authorization.source_entity_type,
                    &authorization.source_entity_id,
                )
                .await
                .map_err(|_| PUBLISH_ARTIFACT_STALE_AUTHORIZATION.to_string())?;
            let current_precondition =
                crate::entity_actor::effects::entity_authorization_precondition(
                    &snapshot.current_state.state,
                );
            if current_precondition != authorization.state_precondition
                || snapshot.resource_attrs != authorization.resource_attrs
            {
                return Err(PUBLISH_ARTIFACT_STALE_AUTHORIZATION.to_string());
            }
        }

        let public_base_url = self
            .secret(tenant, "published_blob_public_base_url")
            .ok_or_else(|| "missing published_blob_public_base_url secret".to_string())?;
        let endpoint = self
            .secret(tenant, "published_blob_endpoint")
            .ok_or_else(|| "missing published_blob_endpoint secret".to_string())?;
        let bucket = self
            .secret(tenant, "published_blob_bucket")
            .unwrap_or_else(|| DEFAULT_PUBLIC_ARTIFACT_NAMESPACE.to_string());
        let namespace = request
            .namespace
            .as_deref()
            .filter(|namespace| !namespace.trim().is_empty())
            .unwrap_or(DEFAULT_PUBLIC_ARTIFACT_NAMESPACE);
        let (endpoint_host, _) = parse_url_host_path(endpoint.as_str());
        let span = Span::current();
        span.record("artifact_namespace", namespace);
        span.record("public_blob_bucket", bucket.as_str());
        span.record("public_blob_endpoint_host", endpoint_host);

        let storage_key = public_storage_key(
            namespace,
            &request.owner_ref_type,
            &request.owner_ref_id,
            &request.label,
            &content_hash,
            &mime_type,
        )?;
        put_public_blob(
            self,
            tenant,
            endpoint.as_str(),
            bucket.as_str(),
            &storage_key,
            &mime_type,
            &bytes,
        )
        .await?;

        let public_url = format!(
            "{}/{}",
            public_base_url.trim_end_matches('/'),
            storage_key.trim_start_matches('/')
        );
        let artifact_id = published_artifact_id(
            tenant.as_str(),
            &request.owner_ref_type,
            &request.owner_ref_id,
            &request.label,
            &content_hash,
        );
        let artifact = PublishedArtifactStoreUpsert {
            id: artifact_id,
            tenant: tenant.to_string(),
            source_file_id: request.file_id,
            source_file_version_id: request.source_file_version_id,
            content_hash,
            label: request.label,
            mime_type,
            byte_length: bytes.len() as i64,
            public_storage_key: storage_key,
            public_url,
            owner_ref_type: request.owner_ref_type,
            owner_ref_id: request.owner_ref_id,
            status: "published".to_string(),
        };

        let store = self
            .metadata_store_for_tenant(tenant.as_str())
            .await
            .ok_or_else(|| "published artifact metadata store unavailable".to_string())?;
        let persisted = store
            .upsert_published_artifact(&artifact)
            .await
            .map_err(|e| format!("failed to persist published artifact: {e}"))?;
        let telemetry = PublishedArtifactTelemetry::from_row(
            &persisted,
            store.backend_name(),
            namespace,
            bucket.as_str(),
            endpoint_host,
        );
        telemetry.record_on_current_span();
        emit_published_artifact_persisted_log(&telemetry);
        Ok(persisted)
    }

    fn secret(&self, tenant: &TenantId, key: &str) -> Option<String> {
        self.secrets_vault
            .as_ref()
            .and_then(|vault| vault.get_secret(tenant.as_str(), key))
    }
}

#[instrument(skip_all, fields(
    otel.name = "state.put_public_blob",
    tenant = %tenant,
    bucket = %bucket,
    storage_key = %storage_key,
    endpoint_host = tracing::field::Empty,
    mime_type = %stream_content_type(mime_type),
    byte_length = bytes.len(),
    http.status_code = tracing::field::Empty,
))]
async fn put_public_blob(
    state: &ServerState,
    tenant: &TenantId,
    endpoint: &str,
    bucket: &str,
    storage_key: &str,
    mime_type: &str,
    bytes: &[u8],
) -> Result<(), String> {
    if crate::blob_store::is_local_internal_blob_endpoint(endpoint) {
        let object_key = format!(
            "{}/{}",
            bucket.trim_matches('/'),
            storage_key.trim_start_matches('/')
        );
        state
            .put_blob_object(tenant, &object_key, bytes, None)
            .await
            .map_err(|error| {
                format!(
                    "direct tenant-scoped public blob write failed for bucket '{bucket}' key '{storage_key}': {error}"
                )
            })?;
        tracing::Span::current().record("http.status_code", 204_u16);
        tracing::info!(
            tenant = %tenant,
            bucket,
            storage_key,
            mime_type = %stream_content_type(mime_type),
            byte_length = bytes.len(),
            "public blob stored through tenant-scoped local API"
        );
        return Ok(());
    }

    let url = format!(
        "{}/{}/{}",
        endpoint.trim_end_matches('/'),
        bucket.trim_matches('/'),
        storage_key.trim_start_matches('/')
    );
    let (endpoint_host, _) = parse_url_host_path(&url);
    tracing::Span::current().record("endpoint_host", endpoint_host);
    let mut request = public_blob_http_client()
        .put(&url)
        .body(bytes.to_vec())
        .header(CONTENT_TYPE, stream_content_type(mime_type));
    let headers = build_public_blob_put_headers(state, tenant, &url, mime_type, bytes)?;
    for (header_name, header_value) in &headers {
        request = request.header(header_name, header_value);
    }

    let response = request
        .send()
        .await
        .map_err(|e| public_blob_put_request_error(endpoint, bucket, storage_key, &e))?;
    let status = response.status();
    tracing::Span::current().record("http.status_code", status.as_u16());
    if !status.is_success() {
        let error = public_blob_put_status_error(bucket, endpoint, storage_key, status);
        let mime_type = stream_content_type(mime_type);
        tracing::warn!(
            http.status_code = %status,
            endpoint_host,
            bucket,
            storage_key,
            mime_type,
            byte_length = bytes.len(),
            "public blob PUT failed"
        );
        return Err(error);
    }
    let mime_type = stream_content_type(mime_type);
    tracing::info!(
        http.status_code = %status,
        endpoint_host,
        bucket,
        storage_key,
        mime_type,
        byte_length = bytes.len(),
        "public blob PUT succeeded"
    );
    Ok(())
}

fn public_blob_put_request_error(
    endpoint: &str,
    bucket: &str,
    storage_key: &str,
    error: &reqwest::Error,
) -> String {
    let (endpoint_host, _) = parse_url_host_path(endpoint);
    format!(
        "public blob PUT request failed for bucket '{bucket}' key '{storage_key}' via endpoint host '{endpoint_host}': {error}"
    )
}

fn public_blob_put_status_error(
    bucket: &str,
    endpoint: &str,
    storage_key: &str,
    status: reqwest::StatusCode,
) -> String {
    let (endpoint_host, _) = parse_url_host_path(endpoint);
    format!(
        "public blob PUT failed for bucket '{bucket}' key '{storage_key}' via endpoint host '{endpoint_host}' with HTTP {status}"
    )
}

fn build_public_blob_put_headers(
    state: &ServerState,
    tenant: &TenantId,
    url: &str,
    mime_type: &str,
    bytes: &[u8],
) -> Result<HeaderMap, String> {
    let headers = HeaderMap::new();
    let access_key = state
        .secret(tenant, "published_blob_access_key")
        .or_else(|| state.secret(tenant, "blob_access_key"));
    let secret_key = state
        .secret(tenant, "published_blob_secret_key")
        .or_else(|| state.secret(tenant, "blob_secret_key"));
    let (Some(access_key), Some(secret_key)) = (access_key, secret_key) else {
        return Ok(headers);
    };

    let payload_hash = sha256_hex(bytes);
    let amz_date = aws_sigv4::amz_date_now();
    let content_type = stream_content_type(mime_type);
    aws_sigv4::build_signed_headers(&aws_sigv4::SignedHeaderRequest {
        method: "PUT",
        url,
        payload_hash: &payload_hash,
        region: "auto",
        service: "s3",
        access_key: &access_key,
        secret_key: &secret_key,
        amz_date: &amz_date,
        extra_signed_headers: &[("content-type", &content_type)],
        error_context: "public blob",
    })
}

fn public_storage_key(
    namespace: &str,
    owner_ref_type: &str,
    owner_ref_id: &str,
    label: &str,
    content_hash: &str,
    mime_type: &str,
) -> Result<String, String> {
    let hash = content_hash.trim_start_matches("sha256:");
    validate_path_segment("namespace", namespace)?;
    validate_path_segment("owner_ref_type", owner_ref_type)?;
    validate_path_segment("owner_ref_id", owner_ref_id)?;
    validate_path_segment("label", label)?;
    validate_path_segment("content_hash", hash)?;
    Ok(format!(
        "{}/{}/{}/{}-{}.{}",
        namespace,
        owner_ref_type,
        owner_ref_id,
        label,
        hash,
        extension_for_mime(mime_type)
    ))
}

fn published_artifact_id(
    tenant: &str,
    owner_ref_type: &str,
    owner_ref_id: &str,
    label: &str,
    content_hash: &str,
) -> String {
    let seed = format!("{tenant}\n{owner_ref_type}\n{owner_ref_id}\n{label}\n{content_hash}");
    let digest = sha256_hex(seed.as_bytes());
    format!("part-{}", &digest[..32])
}

fn validate_path_segment(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value == "." || value == ".." {
        return Err(format!("{field} must not be a relative path segment"));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~'))
    {
        return Err(format!(
            "{field} must be one URI-safe path segment using only letters, digits, '-', '_', '.' or '~'"
        ));
    }
    Ok(())
}

fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type.split(';').next().unwrap_or("").trim() {
        "text/html" => "html",
        "text/css" => "css",
        "text/javascript" | "application/javascript" => "js",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "application/json" => "json",
        "text/markdown" => "md",
        _ => "bin",
    }
}

fn stream_content_type(mime_type: &str) -> String {
    if mime_type.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        mime_type.to_string()
    }
}

fn public_blob_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

#[cfg(test)]
mod tests;
