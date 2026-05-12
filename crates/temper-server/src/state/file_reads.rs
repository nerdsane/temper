use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use super::ServerState;
use super::file_read_projection::{
    FileProjectionMeta, file_projection_from_row, file_projection_from_state,
    file_version_projection_from_row, file_version_projection_from_state,
};

const FILE_BATCH_READ_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TextFileReadResult {
    pub file_id: String,
    pub found: bool,
    pub content_hash: String,
    pub mime_type: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TextFileVersionReadResult {
    pub file_version_id: String,
    pub found: bool,
    pub content_hash: String,
    pub mime_type: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexedFileStreamRead {
    MissingIndex,
    StaleIndex {
        content_hash: String,
        mime_type: String,
    },
    NoContent {
        content_hash: String,
        mime_type: String,
    },
    Content {
        content_hash: String,
        mime_type: String,
        bytes: Vec<u8>,
    },
}

impl ServerState {
    #[instrument(skip_all, fields(
        otel.name = "state.read_file_stream_indexed",
        tenant = %tenant,
        file_id,
    ))]
    pub async fn read_file_stream_indexed(
        &self,
        tenant: &TenantId,
        file_id: &str,
    ) -> Result<IndexedFileStreamRead, String> {
        let meta_by_id = self
            .load_file_projection_metadata_from_index(tenant, &[file_id.to_string()])
            .await?;
        let Some(meta) = meta_by_id.get(file_id).cloned() else {
            return Ok(IndexedFileStreamRead::MissingIndex);
        };
        self.read_indexed_stream_from_meta(tenant, meta).await
    }

    #[instrument(skip_all, fields(
        otel.name = "state.read_file_version_stream_indexed",
        tenant = %tenant,
        file_version_id,
    ))]
    pub async fn read_file_version_stream_indexed(
        &self,
        tenant: &TenantId,
        file_version_id: &str,
    ) -> Result<IndexedFileStreamRead, String> {
        let meta_by_id = self
            .load_file_version_projection_metadata_from_index(
                tenant,
                &[file_version_id.to_string()],
            )
            .await?;
        let Some(meta) = meta_by_id.get(file_version_id).cloned() else {
            return Ok(IndexedFileStreamRead::MissingIndex);
        };
        self.read_indexed_stream_from_meta(tenant, meta).await
    }

    #[instrument(skip_all, fields(
        otel.name = "state.read_file_texts_batch",
        tenant = %tenant,
        file_count = file_ids.len(),
    ))]
    pub async fn read_file_texts_batch(
        &self,
        tenant: &TenantId,
        file_ids: &[String],
    ) -> Result<Vec<TextFileReadResult>, String> {
        if file_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut unique_ids = Vec::new();
        let mut seen = BTreeSet::new();
        for file_id in file_ids {
            if seen.insert(file_id.clone()) {
                unique_ids.push(file_id.clone());
            }
        }

        let meta_by_id = self
            .load_file_projection_metadata_batch(tenant, &unique_ids)
            .await?;

        let meta_by_id = Arc::new(meta_by_id);
        let results = stream::iter(file_ids.iter().cloned())
            .map(|file_id| {
                let meta_by_id = Arc::clone(&meta_by_id);
                async move {
                    let meta = meta_by_id.get(&file_id).cloned();
                    self.read_single_file_text(tenant, file_id, meta).await
                }
            })
            .buffered(FILE_BATCH_READ_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        let mut out = Vec::with_capacity(results.len());
        for result in results {
            out.push(result?);
        }
        Ok(out)
    }

    #[instrument(skip_all, fields(
        otel.name = "state.read_file_version_texts_batch",
        tenant = %tenant,
        file_version_count = file_version_ids.len(),
    ))]
    pub async fn read_file_version_texts_batch(
        &self,
        tenant: &TenantId,
        file_version_ids: &[String],
    ) -> Result<Vec<TextFileVersionReadResult>, String> {
        if file_version_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut unique_ids = Vec::new();
        let mut seen = BTreeSet::new();
        for file_version_id in file_version_ids {
            if seen.insert(file_version_id.clone()) {
                unique_ids.push(file_version_id.clone());
            }
        }

        let meta_by_id = self
            .load_file_version_projection_metadata_batch(tenant, &unique_ids)
            .await?;

        let meta_by_id = Arc::new(meta_by_id);
        let results = stream::iter(file_version_ids.iter().cloned())
            .map(|file_version_id| {
                let meta_by_id = Arc::clone(&meta_by_id);
                async move {
                    let meta = meta_by_id.get(&file_version_id).cloned();
                    self.read_single_file_version_text(tenant, file_version_id, meta)
                        .await
                }
            })
            .buffered(FILE_BATCH_READ_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        let mut out = Vec::with_capacity(results.len());
        for result in results {
            out.push(result?);
        }
        Ok(out)
    }

    async fn load_file_projection_metadata_batch(
        &self,
        tenant: &TenantId,
        file_ids: &[String],
    ) -> Result<BTreeMap<String, FileProjectionMeta>, String> {
        let mut by_id = self
            .load_file_projection_metadata_from_index(tenant, file_ids)
            .await?;

        let missing_ids: Vec<String> = file_ids
            .iter()
            .filter(|file_id| !by_id.contains_key(*file_id))
            .cloned()
            .collect();

        for file_id in missing_ids {
            if let Ok(resp) = self.get_tenant_entity_state(tenant, "File", &file_id).await {
                by_id.insert(file_id, file_projection_from_state(&resp.state.fields));
            }
        }

        Ok(by_id)
    }

    async fn load_file_version_projection_metadata_batch(
        &self,
        tenant: &TenantId,
        file_version_ids: &[String],
    ) -> Result<BTreeMap<String, FileProjectionMeta>, String> {
        let mut by_id = self
            .load_file_version_projection_metadata_from_index(tenant, file_version_ids)
            .await?;

        let missing_ids: Vec<String> = file_version_ids
            .iter()
            .filter(|file_version_id| !by_id.contains_key(*file_version_id))
            .cloned()
            .collect();

        for file_version_id in missing_ids {
            if let Ok(resp) = self
                .get_tenant_entity_state(tenant, "FileVersion", &file_version_id)
                .await
            {
                by_id.insert(
                    file_version_id,
                    file_version_projection_from_state(&resp.state.fields),
                );
            }
        }

        Ok(by_id)
    }

    async fn load_file_projection_metadata_from_index(
        &self,
        tenant: &TenantId,
        file_ids: &[String],
    ) -> Result<BTreeMap<String, FileProjectionMeta>, String> {
        let mut by_id = BTreeMap::new();

        if let Some(query_plane) = self.query_plane_store() {
            let rows = query_plane
                .load_projection_fields_many(
                    tenant.as_str(),
                    "File",
                    file_ids,
                    &["content_hash", "mime_type", "has_content"],
                )
                .await
                .map_err(|e| format!("failed to load File projections: {e}"))?
                .unwrap_or_default();
            for row in rows {
                by_id.insert(row.entity_id.clone(), file_projection_from_row(row));
            }
        }

        Ok(by_id)
    }

    async fn load_file_version_projection_metadata_from_index(
        &self,
        tenant: &TenantId,
        file_version_ids: &[String],
    ) -> Result<BTreeMap<String, FileProjectionMeta>, String> {
        let mut by_id = BTreeMap::new();

        if let Some(query_plane) = self.query_plane_store() {
            let rows = query_plane
                .load_projection_fields_many(
                    tenant.as_str(),
                    "FileVersion",
                    file_version_ids,
                    &["content_hash", "mime_type"],
                )
                .await
                .map_err(|e| format!("failed to load FileVersion projections: {e}"))?
                .unwrap_or_default();
            for row in rows {
                by_id.insert(row.entity_id.clone(), file_version_projection_from_row(row));
            }
        }

        Ok(by_id)
    }

    async fn read_indexed_stream_from_meta(
        &self,
        tenant: &TenantId,
        meta: FileProjectionMeta,
    ) -> Result<IndexedFileStreamRead, String> {
        if !meta.has_content || meta.content_hash.is_empty() {
            return Ok(IndexedFileStreamRead::NoContent {
                content_hash: meta.content_hash,
                mime_type: meta.mime_type,
            });
        }

        let Some(bytes) = self
            .fetch_blob_bytes_for_hash(tenant, &meta.content_hash)
            .await?
        else {
            return Ok(IndexedFileStreamRead::StaleIndex {
                content_hash: meta.content_hash,
                mime_type: meta.mime_type,
            });
        };

        Ok(IndexedFileStreamRead::Content {
            content_hash: meta.content_hash,
            mime_type: meta.mime_type,
            bytes,
        })
    }

    async fn read_single_file_text(
        &self,
        tenant: &TenantId,
        file_id: String,
        meta: Option<FileProjectionMeta>,
    ) -> Result<TextFileReadResult, String> {
        let Some(meta) = meta else {
            return Ok(TextFileReadResult {
                file_id,
                found: false,
                content_hash: String::new(),
                mime_type: String::new(),
                text: String::new(),
            });
        };

        if !meta.has_content || meta.content_hash.is_empty() {
            return Ok(TextFileReadResult {
                file_id,
                found: true,
                content_hash: meta.content_hash,
                mime_type: meta.mime_type,
                text: String::new(),
            });
        }

        let text = self
            .fetch_blob_text_for_hash(tenant, &meta.content_hash)
            .await?
            .unwrap_or_default();

        Ok(TextFileReadResult {
            file_id,
            found: true,
            content_hash: meta.content_hash,
            mime_type: meta.mime_type,
            text,
        })
    }

    async fn read_single_file_version_text(
        &self,
        tenant: &TenantId,
        file_version_id: String,
        meta: Option<FileProjectionMeta>,
    ) -> Result<TextFileVersionReadResult, String> {
        let Some(meta) = meta else {
            return Ok(TextFileVersionReadResult {
                file_version_id,
                found: false,
                content_hash: String::new(),
                mime_type: String::new(),
                text: String::new(),
            });
        };

        if !meta.has_content || meta.content_hash.is_empty() {
            return Ok(TextFileVersionReadResult {
                file_version_id,
                found: true,
                content_hash: meta.content_hash,
                mime_type: meta.mime_type,
                text: String::new(),
            });
        }

        let text = self
            .fetch_blob_text_for_hash(tenant, &meta.content_hash)
            .await?
            .unwrap_or_default();

        Ok(TextFileVersionReadResult {
            file_version_id,
            found: true,
            content_hash: meta.content_hash,
            mime_type: meta.mime_type,
            text,
        })
    }

    async fn fetch_blob_text_for_hash(
        &self,
        tenant: &TenantId,
        content_hash: &str,
    ) -> Result<Option<String>, String> {
        let blob_bytes = self.fetch_blob_bytes_for_hash(tenant, content_hash).await?;
        Ok(blob_bytes.map(|bytes| String::from_utf8_lossy(&bytes).to_string()))
    }

    async fn fetch_blob_bytes_for_hash(
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
