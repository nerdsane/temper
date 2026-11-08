use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, registry::LookupSpan};

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_store_turso::TursoEventStore;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::aws_sigv4::sha256_hex;
use crate::registry::SpecRegistry;
use crate::secrets::vault::SecretsVault;
use crate::state::{ServerState, native_file_blob_key};
use crate::storage::{PublishedArtifactStoreRow, StorageStack};

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
        &TenantId::new("tenant-a"),
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
        format!(
            "public-demo-artifacts/tenants/{}/Report/quarterly-2026/preview-image-abc123.png",
            sha256_hex(b"tenant-a")
        )
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
        "app-published-assets",
        "https://075a5c0a617de3bdc08a44f9794b6f2f.r2.cloudflarestorage.com",
        "published-artifacts/CodexProof/demo/report.md",
        reqwest::StatusCode::FORBIDDEN,
    );

    assert_eq!(
        error,
        "public blob PUT failed for bucket 'app-published-assets' key \
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

/// A server whose tenants publish to one shared public endpoint, each keeping its File bytes in its own local store.
struct Publishing {
    state: ServerState,
    store: TursoEventStore,
    _dir: tempfile::TempDir,
}

async fn publishing(endpoint: &MockServer, tenants: &[&TenantId]) -> Publishing {
    let dir = tempfile::tempdir().expect("data dir");
    let db_url = format!("file:{}", dir.path().join("publishing.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("local turso db");
    let vault = SecretsVault::new(&[9u8; 32]);
    for tenant in tenants {
        vault.cache_secret(
            tenant.as_str(),
            "published_blob_public_base_url",
            format!("{}/public", endpoint.uri()),
        );
        vault.cache_secret(tenant.as_str(), "published_blob_endpoint", endpoint.uri());
    }
    let mut state = ServerState::from_registry(
        ActorSystem::new("published-artifact-keys"),
        SpecRegistry::new(),
    )
    .with_secrets_vault(vault);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state.data_dir = dir.path().to_path_buf();
    Publishing {
        state,
        store,
        _dir: dir,
    }
}

impl Publishing {
    /// Keep `bytes` as `tenant`'s File `file_id`, recorded with `content_hash`.
    async fn file(&self, tenant: &TenantId, file_id: &str, content_hash: &str, bytes: &[u8]) {
        self.state
            .put_file_content_blob(tenant, &native_file_blob_key(content_hash), bytes)
            .await
            .expect("keep File bytes");
        self.store
            .upsert_query_projection(
                tenant.as_str(),
                "File",
                file_id,
                "Ready",
                &serde_json::json!({
                    "content_hash": content_hash,
                    "mime_type": "text/markdown",
                    "has_content": true,
                }),
                1,
            )
            .await
            .expect("File projection");
    }

    async fn publish(
        &self,
        tenant: &TenantId,
        file_id: &str,
    ) -> Result<PublishedArtifactStoreRow, String> {
        self.state
            .publish_file_artifact(
                tenant,
                PublishFileArtifactRequest {
                    file_id: file_id.to_string(),
                    label: "latest".to_string(),
                    owner_ref_type: "Doc".to_string(),
                    owner_ref_id: "doc-a".to_string(),
                    source_file_version_id: String::new(),
                    namespace: Some("publish-test".to_string()),
                },
            )
            .await
    }
}

async fn public_endpoint(puts: u64) -> MockServer {
    let endpoint = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(puts)
        .mount(&endpoint)
        .await;
    endpoint
}

#[tokio::test]
async fn a_published_artifact_is_written_only_with_bytes_that_match_its_content_hash() {
    let endpoint = public_endpoint(0).await;
    let tenant = TenantId::new("tenant-a");
    let publishing = publishing(&endpoint, &[&tenant]).await;
    let recorded = format!("sha256:{}", sha256_hex(b"the bytes the hash names"));
    publishing
        .file(&tenant, "file-a", &recorded, b"other bytes")
        .await;

    let published = publishing.publish(&tenant, "file-a").await;

    assert!(
        published.is_err(),
        "bytes that do not hash to the recorded content hash are not published: {published:?}"
    );
    endpoint.verify().await;
}

#[tokio::test]
async fn tenants_publishing_the_same_owner_label_and_hash_get_different_keys() {
    let endpoint = public_endpoint(2).await;
    let (tenant_a, tenant_b) = (TenantId::new("tenant-a"), TenantId::new("tenant-b"));
    let publishing = publishing(&endpoint, &[&tenant_a, &tenant_b]).await;
    let bytes = b"the same bytes";
    let content_hash = format!("sha256:{}", sha256_hex(bytes));
    for tenant in [&tenant_a, &tenant_b] {
        publishing
            .file(tenant, "file-a", &content_hash, bytes)
            .await;
    }

    let first = publishing
        .publish(&tenant_a, "file-a")
        .await
        .expect("tenant A publishes");
    let second = publishing
        .publish(&tenant_b, "file-a")
        .await
        .expect("tenant B publishes");

    assert_ne!(first.public_storage_key, second.public_storage_key);
    assert_ne!(first.public_url, second.public_url);
    for (artifact, tenant) in [(&first, &tenant_a), (&second, &tenant_b)] {
        let tenant_segment = format!("/tenants/{}/", sha256_hex(tenant.as_str().as_bytes()));
        assert!(
            artifact.public_storage_key.contains(&tenant_segment),
            "{} carries {tenant}",
            artifact.public_storage_key
        );
    }
    endpoint.verify().await;
}
