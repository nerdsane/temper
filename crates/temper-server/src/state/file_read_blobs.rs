use temper_runtime::tenant::TenantId;

use super::ServerState;

impl ServerState {
    #[cfg(feature = "observe")]
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
        for blob_key in file_blob_read_keys(blob_endpoint.as_deref(), content_hash) {
            let blob_bytes = self
                .get_blob_with_legacy_fallback(tenant, &blob_key)
                .await
                .map_err(|e| format!("failed to read blob '{content_hash}': {e}"))?;
            if blob_bytes.is_some() {
                return Ok(blob_bytes);
            }
        }
        Ok(None)
    }
}

fn file_blob_read_keys(blob_endpoint: Option<&str>, content_hash: &str) -> Vec<String> {
    let native_key = native_file_blob_key(content_hash);
    let mut keys = vec![native_key.clone()];

    if blob_endpoint
        .as_ref()
        .is_some_and(|endpoint| !crate::blob_store::is_local_internal_blob_endpoint(endpoint))
    {
        // The legacy blob_adapter WASM writes external R2/S3 objects at
        // `{bucket}/{content_hash}` because the external provider endpoint
        // already carries the bucket namespace.
        let legacy_external_key = content_hash.to_string();
        if legacy_external_key != native_key {
            keys.push(legacy_external_key);
        }
    }

    keys
}

fn native_file_blob_key(content_hash: &str) -> String {
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
    fn file_blob_read_keys_prefer_native_key_for_external_endpoint() {
        assert_eq!(
            file_blob_read_keys(
                Some("https://example.r2.cloudflarestorage.com"),
                "sha256:abc"
            ),
            vec!["temper-fs/sha256:abc".to_string(), "sha256:abc".to_string()]
        );
    }

    #[test]
    fn file_blob_read_keys_keep_single_native_key_for_internal_store() {
        assert_eq!(
            file_blob_read_keys(Some("http://127.0.0.1:4491/_internal/blobs"), "sha256:abc"),
            vec!["temper-fs/sha256:abc".to_string()]
        );
        assert_eq!(
            file_blob_read_keys(None, "sha256:abc"),
            vec!["temper-fs/sha256:abc".to_string()]
        );
    }
}
