use temper_runtime::tenant::TenantId;

use super::ServerState;
use super::file_writes::native_file_blob_key;

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
        // A server that keeps Files only in an object store reads none from a local store, which other servers on the
        // machine may share: only its own database copy, from before.
        let own_only = self.files_refused_here(tenant).is_some();
        for blob_key in file_blob_read_keys(blob_endpoint.as_deref(), content_hash) {
            let read = if own_only {
                self.get_own_database_blob(tenant, &blob_key).await
            } else {
                self.get_blob_with_legacy_fallback(tenant, &blob_key).await
            };
            let blob_bytes =
                read.map_err(|e| format!("failed to read blob '{content_hash}': {e}"))?;
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

    fn server(name: &str, data_dir: &std::path::Path) -> ServerState {
        let mut state = ServerState::new(
            temper_runtime::ActorSystem::new(name),
            temper_spec::csdl::CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        );
        state.data_dir = data_dir.to_path_buf();
        state
    }

    /// Two servers on one machine share its local store (each default tenant's is `$HOME/.local/share/temper/blobs`).
    /// One that keeps Files only in an object store does not serve bytes the other left there; the one that stored
    /// them finds them by their hash.
    #[tokio::test]
    async fn a_server_that_keeps_files_only_in_an_object_store_reads_none_from_a_shared_local_store()
     {
        let shared = tempfile::tempdir().expect("data dir");
        let tenant = TenantId::default();
        let other = server("files-other-app", shared.path());
        let (hash, key) =
            super::super::file_writes::content_hash_and_native_blob_key(b"another app's bytes");
        other
            .put_file_content_blob(&tenant, &key, b"another app's bytes")
            .await
            .expect("kept locally");
        assert_eq!(
            other
                .fetch_blob_bytes_for_hash(&tenant, &hash)
                .await
                .expect("read")
                .as_deref(),
            Some(&b"another app's bytes"[..])
        );

        let mut app = server("files-this-app", shared.path());
        app.files_need_object_store = true;
        assert_eq!(
            app.fetch_blob_bytes_for_hash(&tenant, &hash)
                .await
                .expect("read"),
            None
        );
        assert!(app.files_refused_here(&tenant).is_some());
    }
}
