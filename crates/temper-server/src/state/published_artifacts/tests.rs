use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, registry::LookupSpan};

use crate::storage::PublishedArtifactStoreRow;

use super::{
    PublishFileArtifactRequest, PublishedArtifactTelemetry, emit_published_artifact_persisted_log,
    public_blob_put_status_error, public_storage_key, published_artifact_id, validate_path_segment,
};

#[derive(Clone, Default)]
struct CapturedEvents {
    fields: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
}

impl<S> Layer<S> for CapturedEvents
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.fields.lock().unwrap().push(visitor.fields);
    }
}

#[derive(Default)]
struct FieldVisitor {
    fields: BTreeMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[test]
fn public_storage_key_is_generic_and_content_addressed() {
    let key = public_storage_key(
        "public-demo-artifacts",
        "Report",
        "quarterly-2026",
        "preview-image",
        "sha256:abc123",
        "image/png",
    )
    .expect("valid path segments");

    assert_eq!(
        key,
        "public-demo-artifacts/Report/quarterly-2026/preview-image-abc123.png"
    );
}

#[test]
fn artifact_path_segments_reject_breakout_and_ambiguous_values() {
    for value in ["", ".", "..", "a/b", r"a\b", "line\nbreak", "has space"] {
        assert!(
            validate_path_segment("label", value).is_err(),
            "segment should be rejected: {value:?}"
        );
    }
    for value in ["artifact", "artifact_1", "Artifact-2026", "v1.2"] {
        assert!(
            validate_path_segment("label", value).is_ok(),
            "segment should be accepted: {value:?}"
        );
    }
}

#[test]
fn publish_request_rejects_invalid_namespace_owner_and_label() {
    let valid = PublishFileArtifactRequest {
        file_id: "file-a".to_string(),
        label: "latest".to_string(),
        owner_ref_type: "Document".to_string(),
        owner_ref_id: "doc-a".to_string(),
        source_file_version_id: String::new(),
        namespace: Some("published-artifacts".to_string()),
    };
    assert!(valid.validate().is_ok());

    for invalid in [
        PublishFileArtifactRequest {
            namespace: Some("../escape".to_string()),
            ..valid.clone()
        },
        PublishFileArtifactRequest {
            owner_ref_id: "owner/escape".to_string(),
            ..valid.clone()
        },
        PublishFileArtifactRequest {
            label: "..".to_string(),
            ..valid.clone()
        },
    ] {
        assert!(invalid.validate().is_err());
    }
}

#[test]
fn published_artifact_id_uses_generic_owner_ref_label_and_hash() {
    let first = published_artifact_id(
        "tenant-a",
        "Report",
        "quarterly-2026",
        "preview",
        "sha256:abc123",
    );
    let second = published_artifact_id(
        "tenant-a",
        "Report",
        "quarterly-2026",
        "download",
        "sha256:abc123",
    );

    assert!(first.starts_with("part-"));
    assert_eq!(first.len(), "part-".len() + 32);
    assert_ne!(first, second);
}

#[test]
fn public_blob_put_status_error_names_bucket_key_and_endpoint_host() {
    let error = public_blob_put_status_error(
        "katagami-published-assets",
        "https://075a5c0a617de3bdc08a44f9794b6f2f.r2.cloudflarestorage.com",
        "published-artifacts/CodexProof/demo/report.md",
        reqwest::StatusCode::FORBIDDEN,
    );

    assert_eq!(
        error,
        "public blob PUT failed for bucket 'katagami-published-assets' key \
         'published-artifacts/CodexProof/demo/report.md' via endpoint host \
         '075a5c0a617de3bdc08a44f9794b6f2f.r2.cloudflarestorage.com' with HTTP 403 Forbidden"
    );
}

#[test]
fn published_artifact_success_log_carries_publication_observability_fields() {
    let row = PublishedArtifactStoreRow {
        id: "part-abc".to_string(),
        tenant: "default".to_string(),
        source_file_id: "file-abc".to_string(),
        source_file_version_id: String::new(),
        content_hash: "sha256:abc123".to_string(),
        label: "Datadog proof".to_string(),
        mime_type: "text/markdown".to_string(),
        byte_length: 18568,
        public_storage_key: "codex-live-proof/CodexProof/deploy/proof-abc123.md".to_string(),
        public_url:
            "https://temperpaw-assets.example/codex-live-proof/CodexProof/deploy/proof-abc123.md"
                .to_string(),
        owner_ref_type: "CodexProof".to_string(),
        owner_ref_id: "deploy".to_string(),
        status: "published".to_string(),
    };
    let telemetry = PublishedArtifactTelemetry::from_row(
        &row,
        "postgres",
        "codex-live-proof",
        "published-bucket",
        "r2.example.test",
    );
    let captured = CapturedEvents::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());

    tracing::subscriber::with_default(subscriber, || {
        emit_published_artifact_persisted_log(&telemetry);
    });

    let events = captured.fields.lock().unwrap();
    let event = events
        .iter()
        .find(|fields| {
            fields
                .get("message")
                .is_some_and(|message| message.contains("published artifact metadata persisted"))
        })
        .expect("publish success log event");

    assert_eq!(event.get("tenant").map(String::as_str), Some("default"));
    assert_eq!(
        event.get("artifact_id").map(String::as_str),
        Some("part-abc")
    );
    assert_eq!(
        event.get("source_file_id").map(String::as_str),
        Some("file-abc")
    );
    assert_eq!(
        event.get("source_file_version_id").map(String::as_str),
        Some("")
    );
    assert_eq!(
        event.get("content_hash").map(String::as_str),
        Some("sha256:abc123")
    );
    assert_eq!(
        event.get("artifact_label").map(String::as_str),
        Some("Datadog proof")
    );
    assert_eq!(
        event.get("mime_type").map(String::as_str),
        Some("text/markdown")
    );
    assert_eq!(event.get("byte_length").map(String::as_str), Some("18568"));
    assert_eq!(
        event.get("public_storage_key").map(String::as_str),
        Some("codex-live-proof/CodexProof/deploy/proof-abc123.md")
    );
    assert_eq!(
        event.get("public_url").map(String::as_str),
        Some("https://temperpaw-assets.example/codex-live-proof/CodexProof/deploy/proof-abc123.md")
    );
    assert_eq!(
        event.get("owner_ref_type").map(String::as_str),
        Some("CodexProof")
    );
    assert_eq!(
        event.get("owner_ref_id").map(String::as_str),
        Some("deploy")
    );
    assert_eq!(
        event.get("artifact_status").map(String::as_str),
        Some("published")
    );
    assert_eq!(
        event.get("metadata_backend").map(String::as_str),
        Some("postgres")
    );
    assert_eq!(
        event.get("artifact_namespace").map(String::as_str),
        Some("codex-live-proof")
    );
    assert_eq!(
        event.get("public_blob_bucket").map(String::as_str),
        Some("published-bucket")
    );
    assert_eq!(
        event.get("public_blob_endpoint_host").map(String::as_str),
        Some("r2.example.test")
    );
}
