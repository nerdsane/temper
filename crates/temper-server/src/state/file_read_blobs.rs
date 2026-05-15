use temper_runtime::tenant::TenantId;

use super::ServerState;

impl ServerState {
    pub(crate) async fn fetch_blob_text_for_hash(
        &self,
        tenant: &TenantId,
        content_hash: &str,
    ) -> Result<Option<String>, String> {
        let blob_bytes = self.fetch_blob_bytes_for_hash(tenant, content_hash).await?;
        Ok(blob_bytes.map(|bytes| String::from_utf8_lossy(&bytes).to_string()))
    }

    pub(crate) async fn fetch_blob_bytes_for_hash(
        &self,
        tenant: &TenantId,
        content_hash: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        let blob_endpoint = self
            .secrets_vault
            .as_ref()
            .and_then(|vault| vault.get_secret(tenant.as_str(), "blob_endpoint"));
        let blob_key = file_blob_key(blob_endpoint.as_deref(), content_hash);
        let mut blob_bytes = self
            .get_blob_with_legacy_fallback(tenant, &blob_key)
            .await
            .map_err(|e| format!("failed to read blob '{content_hash}': {e}"))?;
        if blob_bytes.is_none()
            && blob_endpoint.as_deref().is_some_and(|endpoint| {
                !crate::blob_store::is_local_internal_blob_endpoint(endpoint)
            })
        {
            let legacy_local_key = local_file_blob_key(content_hash);
            if legacy_local_key != blob_key {
                blob_bytes = self
                    .get_blob_with_legacy_fallback(tenant, &legacy_local_key)
                    .await
                    .map_err(|e| {
                        format!("failed to read legacy local blob '{content_hash}': {e}")
                    })?;
            }
        }
        Ok(blob_bytes)
    }
}

fn file_blob_key(blob_endpoint: Option<&str>, content_hash: &str) -> String {
    match blob_endpoint {
        Some(endpoint) if !crate::blob_store::is_local_internal_blob_endpoint(endpoint) => {
            // The blob_adapter WASM writes external R2/S3 objects at
            // `{bucket}/{content_hash}`. Local/internal storage includes the
            // bucket name in the key because the internal route has no
            // separate bucket namespace.
            content_hash.to_string()
        }
        _ => local_file_blob_key(content_hash),
    }
}

fn local_file_blob_key(content_hash: &str) -> String {
    format!(
        "{}/{}",
        crate::blob_store::DEFAULT_BLOB_BUCKET,
        content_hash
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_blob_key_matches_blob_adapter_external_contract() {
        assert_eq!(
            file_blob_key(
                Some("https://example.r2.cloudflarestorage.com"),
                "sha256:abc"
            ),
            "sha256:abc"
        );
    }

    #[test]
    fn file_blob_key_keeps_bucket_prefix_for_internal_store() {
        assert_eq!(
            file_blob_key(Some("http://127.0.0.1:4491/_internal/blobs"), "sha256:abc"),
            "temper-fs/sha256:abc"
        );
        assert_eq!(file_blob_key(None, "sha256:abc"), "temper-fs/sha256:abc");
    }
}
