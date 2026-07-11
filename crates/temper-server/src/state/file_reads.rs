use std::collections::BTreeMap;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use super::ServerState;
use super::file_read_projection::{
    FileProjectionMeta, file_projection_from_row, file_projection_from_state,
    file_version_projection_from_row, file_version_projection_from_state,
};

#[cfg(feature = "observe")]
mod batch_text;

#[cfg(feature = "observe")]
pub(crate) use batch_text::{BatchTextReadError, validate_batch_text_ids};

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

/// Which indexed stream entity a read targets, used to dispatch the
/// authoritative actor-state fallback.
enum StreamKind {
    File,
    Version,
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
            return self
                .read_file_stream_from_state_fallback(tenant, file_id)
                .await;
        };
        let read = self.read_indexed_stream_from_meta(tenant, meta).await?;
        self.reconcile_indexed_stream_read(tenant, StreamKind::File, file_id, read)
            .await
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
            return self
                .read_file_version_stream_from_state_fallback(tenant, file_version_id)
                .await;
        };
        let read = self.read_indexed_stream_from_meta(tenant, meta).await?;
        self.reconcile_indexed_stream_read(tenant, StreamKind::Version, file_version_id, read)
            .await
    }

    /// Reconcile an indexed file stream read against the authoritative actor
    /// state. `StaleIndex` (projected blob missing) and `NoContent` (a version
    /// is recorded but its content bytes are not projected yet — ARN-87) both
    /// indicate a read racing ahead of the projection of a just-committed
    /// cross-actor write, so fall back to the actor state, which holds the
    /// committed content, for read-after-write consistency.
    async fn reconcile_indexed_stream_read(
        &self,
        tenant: &TenantId,
        kind: StreamKind,
        id: &str,
        read: IndexedFileStreamRead,
    ) -> Result<IndexedFileStreamRead, String> {
        match read {
            IndexedFileStreamRead::StaleIndex {
                content_hash,
                mime_type,
            } => {
                let fallback = self.read_stream_state_fallback(tenant, &kind, id).await?;
                if stream_read_content_hash(&fallback).is_some_and(|hash| hash != content_hash) {
                    Ok(fallback)
                } else {
                    Ok(IndexedFileStreamRead::StaleIndex {
                        content_hash,
                        mime_type,
                    })
                }
            }
            IndexedFileStreamRead::NoContent {
                content_hash,
                mime_type,
            } => {
                let fallback = self.read_stream_state_fallback(tenant, &kind, id).await?;
                if matches!(fallback, IndexedFileStreamRead::Content { .. }) {
                    Ok(fallback)
                } else {
                    Ok(IndexedFileStreamRead::NoContent {
                        content_hash,
                        mime_type,
                    })
                }
            }
            read => Ok(read),
        }
    }

    /// Read a file/file-version stream from the authoritative actor state for the
    /// given stream kind.
    async fn read_stream_state_fallback(
        &self,
        tenant: &TenantId,
        kind: &StreamKind,
        id: &str,
    ) -> Result<IndexedFileStreamRead, String> {
        match kind {
            StreamKind::File => self.read_file_stream_from_state_fallback(tenant, id).await,
            StreamKind::Version => {
                self.read_file_version_stream_from_state_fallback(tenant, id)
                    .await
            }
        }
    }

    #[cfg(feature = "observe")]
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

    #[cfg(feature = "observe")]
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

    async fn read_file_stream_from_state_fallback(
        &self,
        tenant: &TenantId,
        file_id: &str,
    ) -> Result<IndexedFileStreamRead, String> {
        let Ok(resp) = self.get_tenant_entity_state(tenant, "File", file_id).await else {
            return Ok(IndexedFileStreamRead::MissingIndex);
        };
        self.read_indexed_stream_from_meta(tenant, file_projection_from_state(&resp.state.fields))
            .await
    }

    async fn read_file_version_stream_from_state_fallback(
        &self,
        tenant: &TenantId,
        file_version_id: &str,
    ) -> Result<IndexedFileStreamRead, String> {
        let Ok(resp) = self
            .get_tenant_entity_state(tenant, "FileVersion", file_version_id)
            .await
        else {
            return Ok(IndexedFileStreamRead::MissingIndex);
        };
        self.read_indexed_stream_from_meta(
            tenant,
            file_version_projection_from_state(&resp.state.fields),
        )
        .await
    }
}

fn stream_read_content_hash(read: &IndexedFileStreamRead) -> Option<&str> {
    match read {
        IndexedFileStreamRead::Content { content_hash, .. }
        | IndexedFileStreamRead::NoContent { content_hash, .. }
        | IndexedFileStreamRead::StaleIndex { content_hash, .. } => Some(content_hash.as_str()),
        IndexedFileStreamRead::MissingIndex => None,
    }
}
