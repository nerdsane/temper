use super::{public_blob_put_status_error, public_storage_key, published_artifact_id};

#[test]
fn public_storage_key_is_generic_and_content_addressed() {
    let key = public_storage_key(
        "public/demo artifacts",
        "Report",
        "quarterly-2026",
        "preview image",
        "sha256:abc123",
        "image/png",
    );

    assert_eq!(
        key,
        "public/demo-artifacts/Report/quarterly-2026/preview-image-abc123.png"
    );
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
