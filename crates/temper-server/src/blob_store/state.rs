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
            let prefix = vault.get_secret(tenant.as_str(), "blob_prefix");
            let link = match (
                vault.get_secret(tenant.as_str(), "blob_link_access_key"),
                vault.get_secret(tenant.as_str(), "blob_link_secret_key"),
            ) {
                (Some(access_key), Some(secret_key)) => Some(super::LinkCredentials {
                    access_key,
                    secret_key,
                    session_token: vault.get_secret(tenant.as_str(), "blob_link_session_token"),
                }),
                _ => None,
            };
            return Ok(BlobStore::s3(
                endpoint,
                bucket,
                vault.get_secret(tenant.as_str(), "blob_access_key"),
                vault.get_secret(tenant.as_str(), "blob_secret_key"),
                self.blob_staging_root(tenant),
                object_key_prefix(prefix.as_deref(), tenant),
            )
            .with_session_token(vault.get_secret(tenant.as_str(), "blob_session_token"))
            .with_link_credentials(link));
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
        if super::kernel_owns(key) {
            return Err(format!(
                "'{key}' is a key the kernel keeps its own records under; nothing was written"
            ));
        }
        self.write_blob_object(tenant, key, body, ttl).await
    }

    /// Write one of the kernel's own blobs ([`super::kernel_owns`]), such as
    /// a module's bytes.
    pub(crate) async fn put_kernel_blob_object(
        &self,
        tenant: &TenantId,
        key: &str,
        body: &[u8],
    ) -> Result<(), String> {
        assert!(super::kernel_owns(key), "'{key}' is not a kernel key");
        self.write_blob_object(tenant, key, body, None).await
    }

    async fn write_blob_object(
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

    /// Write a File's bytes to their content-addressed key. When the tenant's store is a remote object store (R2/S3),
    /// that store alone keeps them: they are never copied into the database, which for an app's own Temper is its one
    /// SQLite file. A local store keeps the database copy the default tenant has always had, unless this server keeps
    /// Files only in an object store (`files_need_object_store`): then nothing is kept and the write is refused.
    pub(crate) async fn put_file_content_blob(
        &self,
        tenant: &TenantId,
        key: &str,
        body: &[u8],
    ) -> Result<(), String> {
        if let Some(why) = self.files_refused_here(tenant) {
            return Err(format!("{why}; nothing was kept"));
        }
        let store = self.blob_store_for_tenant(tenant)?;
        store.put_content_addressed(key, body, None).await?;
        if store.is_object_store() {
            return Ok(());
        }
        self.put_metadata_blob_shadow(tenant, key, body, None).await
    }

    /// Why this server neither keeps nor serves a tenant's File bytes in `store`: it keeps Files only in an object
    /// store (`files_need_object_store`) and `store` is a local one. A local store may be shared by every server on
    /// one machine (`$HOME/.local/share/temper/blobs` for each default tenant), so its bytes are not this server's.
    pub(crate) fn files_refused_here(&self, tenant: &TenantId) -> Option<String> {
        if !self.files_need_object_store {
            return None;
        }
        let remote = self
            .blob_store_for_tenant(tenant)
            .is_ok_and(|store| store.is_object_store());
        (!remote).then(|| {
            format!(
                "this server keeps Files only in an object store, and tenant '{tenant}' has none (no blob_endpoint in its vault)"
            )
        })
    }

    /// A blob from the database copy the default tenant keeps beside a local store, and nowhere else: the copy is
    /// this server's own (an app's Temper has its own database), so it serves Files kept before
    /// `files_need_object_store` without the shared local store.
    pub(crate) async fn get_own_database_blob(
        &self,
        tenant: &TenantId,
        key: &str,
    ) -> Result<Option<Vec<u8>>, String> {
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
    /// The legacy database fallback reads fixed-size chunks too, so the
    /// caller's ceiling is a content boundary, never an allocation budget.
    pub async fn stream_blob_object(
        &self,
        tenant: &TenantId,
        key: &str,
        max_bytes: u64,
    ) -> Result<super::BlobStreamRead, String> {
        match self.blob_store_for_tenant(tenant) {
            Ok(store) => match store.get_stream(key, max_bytes).await {
                Ok(super::BlobStreamRead::Missing) => {}
                other => return other,
            },
            Err(error) => {
                tracing::debug!(
                    %key,
                    %error,
                    "object blob store unavailable for streaming read; trying legacy DB blob fallback"
                );
            }
        }

        // Objects written before the object-store migration live in the legacy
        // DB blob store. `get_blob_with_legacy_fallback` already consults it, so
        // without the same fallback here the streaming and non-streaming reads
        // disagree about which objects exist: a Genesis app bundle reported
        // "field overflow blob ... not found" for a blob the HTTP blob route
        // served successfully at the same moment.
        self.legacy_blob_stream(tenant, key, max_bytes).await
    }

    /// Read one object from the legacy DB blob store as a bounded stream.
    async fn legacy_blob_stream(
        &self,
        tenant: &TenantId,
        key: &str,
        max_bytes: u64,
    ) -> Result<super::BlobStreamRead, String> {
        if tenant != &TenantId::default() {
            return Ok(super::BlobStreamRead::Missing);
        }
        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(super::BlobStreamRead::Missing);
        };
        const CHUNK_BYTES: usize = 64 * 1024;
        // Active downloads use the same stream budget as object storage.
        // Shared blob I/O permits cover database operations only, so a paused
        // client cannot occupy the permits needed by unrelated reads or writes.
        let stream_permit = tokio::time::timeout(
            super::BLOB_IO_QUEUE_TIMEOUT,
            super::streaming::blob_stream_semaphore().acquire_owned(),
        )
        .await
        .map_err(|_| format!("legacy DB blob stream for '{key}' timed out queueing"))?
        .map_err(|error| format!("blob stream semaphore closed for '{key}': {error}"))?;
        // Open with a zero-byte read: this obtains the actual length without
        // fetching content, including for a caller whose ceiling is too small.
        let Some((length, _)) = read_legacy_chunk(store.as_ref(), key, 0, 0).await? else {
            return Ok(super::BlobStreamRead::Missing);
        };
        if length > max_bytes {
            return Ok(super::BlobStreamRead::TooLarge {
                actual_bytes: Some(length),
            });
        }
        let key = key.to_owned();
        let source = Box::pin(async_stream::try_stream! {
            let _stream_permit = stream_permit;
            let mut offset = 0;
            while offset < length {
                let count = (length - offset).min(CHUNK_BYTES as u64) as usize;
                let chunk = read_legacy_chunk(store.as_ref(), &key, offset, count)
                    .await.map_err(std::io::Error::other)?;
                let Some((actual_length, bytes)) = chunk else {
                    Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "legacy blob disappeared during read"))?
                };
                if actual_length != length || bytes.len() != count {
                    Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "legacy blob changed length during read"))?;
                }
                offset += bytes.len() as u64;
                yield bytes::Bytes::from(bytes);
            }
        });
        Ok(super::BlobStreamRead::Found(
            super::BlobObjectStream::from_stream(length, source),
        ))
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

/// Read one chunk while occupying the shared blob I/O budget only for the
/// database operation. No permit escapes this call to a response's `yield`.
async fn read_legacy_chunk(
    store: &dyn crate::storage::MetadataStore,
    key: &str,
    offset: u64,
    count: usize,
) -> Result<Option<(u64, Vec<u8>)>, String> {
    let _io_permit = tokio::time::timeout(
        super::BLOB_IO_QUEUE_TIMEOUT,
        super::blob_io_semaphore().acquire_owned(),
    )
    .await
    .map_err(|_| format!("legacy DB blob read for '{key}' timed out queueing for blob I/O"))?
    .map_err(|error| format!("legacy DB blob I/O semaphore closed for '{key}': {error}"))?;
    tokio::time::timeout(
        super::BLOB_BUFFERED_OPERATION_TIMEOUT,
        store.get_blob_chunk(key, offset, count),
    )
    .await
    .map_err(|_| format!("legacy DB blob read for '{key}' timed out"))?
}

/// Where a tenant's objects sit in the bucket: under the deployment's own prefix (`blob_prefix`, such as one app's
/// folder in a bucket many servers share), then, for any tenant but `default`, under `tenants/<sha256 of its name>`.
fn object_key_prefix(prefix: Option<&str>, tenant: &TenantId) -> Option<String> {
    let prefix = prefix
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty());
    let namespace = tenant_object_namespace(tenant)
        .map(|namespace| format!("{}/{namespace}", super::TENANT_NAMESPACE_SEGMENT));
    match (prefix, namespace) {
        (Some(prefix), Some(namespace)) => Some(format!("{prefix}/{namespace}")),
        (Some(prefix), None) => Some(prefix.to_string()),
        (None, namespace) => namespace,
    }
}

fn tenant_object_namespace(tenant: &TenantId) -> Option<String> {
    (tenant != &TenantId::default())
        .then(|| super::hex_lower(&Sha256::digest(tenant.as_str().as_bytes())))
}

fn tenant_blob_root(root: PathBuf, tenant: &TenantId) -> PathBuf {
    match tenant_object_namespace(tenant) {
        Some(namespace) => root.join(super::TENANT_NAMESPACE_SEGMENT).join(namespace),
        None => root,
    }
}

#[cfg(test)]
#[path = "state_test.rs"]
mod tests;
