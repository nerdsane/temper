use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use temper_runtime::tenant::TenantId;

use super::{BlobReadBounded, BlobStore, DEFAULT_BLOB_BUCKET, is_local_internal_blob_endpoint};
use crate::state::ServerState;

impl ServerState {
    pub(crate) fn blob_store_for_tenant(&self, tenant: &TenantId) -> Result<BlobStore, String> {
        if let Some(vault) = self.secrets_vault.as_ref()
            && let Some(endpoint) = vault.get_secret(tenant.as_str(), "blob_endpoint")
            && !endpoint.trim().is_empty()
        {
            if is_local_internal_blob_endpoint(&endpoint) {
                return self.local_blob_store(tenant).ok_or_else(|| {
                    "internal DB-backed blob endpoint is disabled; set TEMPER_LOCAL_BLOB_DIR or configure BLOB_ENDPOINT for R2/S3"
                        .to_string()
                });
            }
            let bucket = vault
                .get_secret(tenant.as_str(), "blob_bucket")
                .unwrap_or_else(|| DEFAULT_BLOB_BUCKET.to_string());
            return Ok(BlobStore::s3(
                endpoint,
                bucket,
                vault.get_secret(tenant.as_str(), "blob_access_key"),
                vault.get_secret(tenant.as_str(), "blob_secret_key"),
                self.blob_staging_root(tenant),
                tenant_object_namespace(tenant).map(|namespace| format!("tenants/{namespace}")),
            ));
        }
        self.local_blob_store(tenant).ok_or_else(|| {
            "blob object store is not configured; set BLOB_ENDPOINT/BLOB_BUCKET/BLOB_ACCESS_KEY/BLOB_SECRET_KEY or TEMPER_LOCAL_BLOB_DIR"
                .to_string()
        })
    }

    pub(crate) async fn put_blob_object(
        &self,
        tenant: &TenantId,
        key: &str,
        body: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let store = self.blob_store_for_tenant(tenant)?;
        store.put_if_absent(key, body, ttl).await?;
        self.put_metadata_blob_shadow(tenant, key, body, ttl).await
    }

    /// Write bytes to a tenant-scoped content-addressed blob key.
    pub(crate) async fn put_content_addressed_blob(
        &self,
        tenant: &TenantId,
        key: &str,
        body: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let store = self.blob_store_for_tenant(tenant)?;
        store.put_content_addressed(key, body, ttl).await?;
        self.put_metadata_blob_shadow(tenant, key, body, ttl).await
    }

    pub async fn get_blob_with_legacy_fallback(
        &self,
        tenant: &TenantId,
        key: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        match self.blob_store_for_tenant(tenant) {
            Ok(store) => match store.get(key).await {
                Ok(Some(bytes)) => return Ok(Some(bytes)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%key, %error, "object blob store read failed; trying legacy DB blob fallback");
                }
            },
            Err(error) => {
                tracing::debug!(%key, %error, "object blob store unavailable; trying legacy DB blob fallback");
            }
        }
        if tenant != &TenantId::default() {
            return Ok(None);
        }
        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(None);
        };
        store
            .get_blob(key)
            .await
            .map_err(|error| format!("legacy DB blob read failed for '{key}': {error}"))
    }

    pub(crate) async fn get_blob_with_legacy_fallback_bounded(
        &self,
        tenant: &TenantId,
        key: &str,
        max_bytes: usize,
    ) -> Result<BlobReadBounded, String> {
        match self.blob_store_for_tenant(tenant) {
            Ok(store) => match store.get_bounded(key, max_bytes).await {
                Ok(BlobReadBounded::Found(bytes)) => {
                    return Ok(BlobReadBounded::Found(bytes));
                }
                Ok(BlobReadBounded::TooLarge { actual_bytes }) => {
                    return Ok(BlobReadBounded::TooLarge { actual_bytes });
                }
                Ok(BlobReadBounded::Missing) => {}
                Err(error) => {
                    tracing::warn!(%key, %error, "bounded object blob read failed; trying legacy DB fallback");
                }
            },
            Err(error) => {
                tracing::debug!(%key, %error, "object blob store unavailable; trying bounded legacy DB fallback");
            }
        }
        if tenant != &TenantId::default() {
            return Ok(BlobReadBounded::Missing);
        }
        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(BlobReadBounded::Missing);
        };
        store
            .get_blob_if_size_at_most(key, max_bytes)
            .await
            .map(|bytes| match bytes {
                Some(bytes) => BlobReadBounded::Found(bytes),
                None => BlobReadBounded::Missing,
            })
            .map_err(|error| format!("bounded legacy DB blob read failed for '{key}': {error}"))
    }

    /// Open a tenant-scoped object-store blob as a bounded stream.
    ///
    /// Large field-overflow objects are never read from the legacy database
    /// fallback because that interface is buffered; callers receive `Missing`
    /// and can retain the media descriptor instead.
    pub async fn stream_blob_object(
        &self,
        tenant: &TenantId,
        key: &str,
        max_bytes: u64,
    ) -> Result<super::BlobStreamRead, String> {
        self.blob_store_for_tenant(tenant)?
            .get_stream(key, max_bytes)
            .await
    }

    fn local_blob_store(&self, tenant: &TenantId) -> Option<BlobStore> {
        if let Ok(root) = std::env::var("TEMPER_LOCAL_BLOB_DIR") // determinism-ok: deployment config read
            && !root.trim().is_empty()
        {
            return Some(BlobStore::local_fs(tenant_blob_root(root.into(), tenant)));
        }
        if !self.data_dir.as_os_str().is_empty() {
            return Some(BlobStore::local_fs(tenant_blob_root(
                self.data_dir.join("blobs"),
                tenant,
            )));
        }
        None
    }

    fn blob_staging_root(&self, tenant: &TenantId) -> PathBuf {
        let root = if !self.data_dir.as_os_str().is_empty() {
            self.data_dir.join("blob-ingest-staging")
        } else {
            std::env::temp_dir() // determinism-ok: production object-store I/O staging path
                .join("temper-blob-ingest-staging")
        };
        tenant_blob_root(root, tenant)
    }

    async fn put_metadata_blob_shadow(
        &self,
        tenant: &TenantId,
        key: &str,
        body: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        if tenant != &TenantId::default() {
            return Ok(());
        }
        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(());
        };
        store
            .put_blob_with_ttl(key, body, ttl)
            .await
            .map_err(|error| format!("metadata blob shadow write failed for '{key}': {error}"))
    }
}

fn tenant_object_namespace(tenant: &TenantId) -> Option<String> {
    (tenant != &TenantId::default())
        .then(|| super::hex_lower(&Sha256::digest(tenant.as_str().as_bytes())))
}

fn tenant_blob_root(root: PathBuf, tenant: &TenantId) -> PathBuf {
    match tenant_object_namespace(tenant) {
        Some(namespace) => root.join("tenants").join(namespace),
        None => root,
    }
}

#[cfg(test)]
mod tests {
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::CsdlDocument;

    use super::*;

    #[test]
    fn tenant_object_namespaces_are_stable_and_default_compatible() {
        assert_eq!(tenant_object_namespace(&TenantId::default()), None);
        let namespace = tenant_object_namespace(&TenantId::new("tenant-a"))
            .expect("non-default tenant namespace");
        assert_eq!(namespace.len(), 64);
        assert_eq!(
            tenant_object_namespace(&TenantId::new("tenant-a")),
            Some(namespace)
        );
    }

    #[tokio::test]
    async fn local_object_storage_is_namespaced_by_tenant() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let mut state = ServerState::new(
            ActorSystem::new("tenant-blob-isolation"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        );
        state.data_dir = data_dir.path().to_path_buf();
        let tenant_a = TenantId::new("tenant-a");
        let tenant_b = TenantId::new("tenant-b");

        state
            .put_blob_object(&tenant_a, "field-overflow/value", b"tenant-a", None)
            .await
            .expect("tenant A write");
        state
            .put_blob_object(&tenant_b, "field-overflow/value", b"tenant-b", None)
            .await
            .expect("tenant B write");

        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&tenant_a, "field-overflow/value")
                .await
                .expect("tenant A read"),
            Some(b"tenant-a".to_vec())
        );
        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&tenant_b, "field-overflow/value")
                .await
                .expect("tenant B read"),
            Some(b"tenant-b".to_vec())
        );
        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&TenantId::new("tenant-c"), "field-overflow/value",)
                .await
                .expect("tenant C read"),
            None
        );
    }
}
