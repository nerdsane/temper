use tracing::Span;

use crate::storage::PublishedArtifactStoreRow;

#[derive(Debug, Clone)]
pub(super) struct PublishedArtifactTelemetry<'a> {
    tenant: &'a str,
    artifact_id: &'a str,
    source_file_id: &'a str,
    source_file_version_id: &'a str,
    content_hash: &'a str,
    artifact_label: &'a str,
    mime_type: &'a str,
    byte_length: i64,
    public_storage_key: &'a str,
    public_url: &'a str,
    owner_ref_type: &'a str,
    owner_ref_id: &'a str,
    artifact_status: &'a str,
    metadata_backend: &'a str,
    artifact_namespace: &'a str,
    public_blob_bucket: &'a str,
    public_blob_endpoint_host: &'a str,
}

impl<'a> PublishedArtifactTelemetry<'a> {
    pub(super) fn from_row(
        row: &'a PublishedArtifactStoreRow,
        metadata_backend: &'a str,
        artifact_namespace: &'a str,
        public_blob_bucket: &'a str,
        public_blob_endpoint_host: &'a str,
    ) -> Self {
        Self {
            tenant: &row.tenant,
            artifact_id: &row.id,
            source_file_id: &row.source_file_id,
            source_file_version_id: &row.source_file_version_id,
            content_hash: &row.content_hash,
            artifact_label: &row.label,
            mime_type: &row.mime_type,
            byte_length: row.byte_length,
            public_storage_key: &row.public_storage_key,
            public_url: &row.public_url,
            owner_ref_type: &row.owner_ref_type,
            owner_ref_id: &row.owner_ref_id,
            artifact_status: &row.status,
            metadata_backend,
            artifact_namespace,
            public_blob_bucket,
            public_blob_endpoint_host,
        }
    }

    pub(super) fn record_on_current_span(&self) {
        let span = Span::current();
        span.record("artifact_id", self.artifact_id);
        span.record("source_file_id", self.source_file_id);
        span.record("source_file_version_id", self.source_file_version_id);
        span.record("content_hash", self.content_hash);
        span.record("artifact_label", self.artifact_label);
        span.record("mime_type", self.mime_type);
        span.record("byte_length", self.byte_length);
        span.record("public_storage_key", self.public_storage_key);
        span.record("public_url", self.public_url);
        span.record("owner_ref_type", self.owner_ref_type);
        span.record("owner_ref_id", self.owner_ref_id);
        span.record("artifact_status", self.artifact_status);
        span.record("metadata_backend", self.metadata_backend);
        span.record("artifact_namespace", self.artifact_namespace);
        span.record("public_blob_bucket", self.public_blob_bucket);
        span.record("public_blob_endpoint_host", self.public_blob_endpoint_host);
    }
}

pub(super) fn emit_published_artifact_persisted_log(telemetry: &PublishedArtifactTelemetry<'_>) {
    tracing::info!(
        tenant = telemetry.tenant,
        artifact_id = telemetry.artifact_id,
        source_file_id = telemetry.source_file_id,
        source_file_version_id = telemetry.source_file_version_id,
        content_hash = telemetry.content_hash,
        artifact_label = telemetry.artifact_label,
        mime_type = telemetry.mime_type,
        byte_length = telemetry.byte_length,
        public_storage_key = telemetry.public_storage_key,
        public_url = telemetry.public_url,
        owner_ref_type = telemetry.owner_ref_type,
        owner_ref_id = telemetry.owner_ref_id,
        artifact_status = telemetry.artifact_status,
        metadata_backend = telemetry.metadata_backend,
        artifact_namespace = telemetry.artifact_namespace,
        public_blob_bucket = telemetry.public_blob_bucket,
        public_blob_endpoint_host = telemetry.public_blob_endpoint_host,
        "published artifact metadata persisted"
    );
}
