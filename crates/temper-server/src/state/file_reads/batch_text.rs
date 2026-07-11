//! Bounded JSON batch reads for file text.

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;
use tracing::instrument;

use super::{FileProjectionMeta, TextFileReadResult, TextFileVersionReadResult};
use crate::state::ServerState;

const MAX_BATCH_ITEMS: usize = 100;
const MAX_BATCH_ID_BYTES: usize = 512;
const MAX_BATCH_ITEM_BYTES: usize = 2 * 1024 * 1024;
const MAX_BATCH_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Failure returned by a bounded text batch read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchTextReadError {
    /// The caller supplied an invalid identifier list.
    InvalidRequest(String),
    /// The caller exceeded the positional item budget.
    TooManyItems { items: usize },
    /// One item cannot safely be represented by this buffered JSON endpoint.
    ItemTooLarge { id: String, bytes: usize },
    /// The aggregate buffered response would exceed its byte budget.
    ResponseTooLarge { bytes: usize },
    /// A projection or blob backend failed.
    Storage(String),
}

impl std::fmt::Display for BatchTextReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) | Self::Storage(message) => formatter.write_str(message),
            Self::TooManyItems { items } => write!(
                formatter,
                "batch contains {items} items; maximum is {MAX_BATCH_ITEMS}"
            ),
            Self::ItemTooLarge { id, bytes } => write!(
                formatter,
                "item {id:?} is {bytes} bytes; buffered batch items are limited to {MAX_BATCH_ITEM_BYTES} bytes"
            ),
            Self::ResponseTooLarge { bytes } => write!(
                formatter,
                "batch text is {bytes} bytes; buffered batch responses are limited to {MAX_BATCH_RESPONSE_BYTES} bytes"
            ),
        }
    }
}

/// Validate one positional batch without silently changing its result shape.
pub(crate) fn validate_batch_text_ids(ids: &[String]) -> Result<(), BatchTextReadError> {
    if ids.len() > MAX_BATCH_ITEMS {
        return Err(BatchTextReadError::TooManyItems { items: ids.len() });
    }

    let mut seen = BTreeSet::new();
    for id in ids {
        if id.trim().is_empty() {
            return Err(BatchTextReadError::InvalidRequest(
                "batch identifiers must not be empty".to_string(),
            ));
        }
        if id.len() > MAX_BATCH_ID_BYTES {
            return Err(BatchTextReadError::InvalidRequest(format!(
                "batch identifier is {} bytes; maximum is {MAX_BATCH_ID_BYTES}",
                id.len()
            )));
        }
        if !seen.insert(id.as_str()) {
            return Err(BatchTextReadError::InvalidRequest(format!(
                "duplicate batch identifier {id:?}"
            )));
        }
    }
    Ok(())
}

impl ServerState {
    #[instrument(skip_all, fields(
        otel.name = "state.read_file_texts_batch",
        tenant = %tenant,
        file_count = file_ids.len(),
    ))]
    pub(crate) async fn read_file_texts_batch(
        &self,
        tenant: &TenantId,
        file_ids: &[String],
    ) -> Result<Vec<TextFileReadResult>, BatchTextReadError> {
        validate_batch_text_ids(file_ids)?;
        let metadata = self
            .load_file_projection_metadata_batch(tenant, file_ids)
            .await
            .map_err(BatchTextReadError::Storage)?;
        let mut response_bytes = 0usize;
        let mut files = Vec::with_capacity(file_ids.len());
        for file_id in file_ids {
            let result = self
                .read_single_file_text(tenant, file_id.clone(), metadata.get(file_id).cloned())
                .await?;
            consume_response_budget(file_id, result.text.len(), &mut response_bytes)?;
            files.push(result);
        }
        Ok(files)
    }

    #[instrument(skip_all, fields(
        otel.name = "state.read_file_version_texts_batch",
        tenant = %tenant,
        file_version_count = file_version_ids.len(),
    ))]
    pub(crate) async fn read_file_version_texts_batch(
        &self,
        tenant: &TenantId,
        file_version_ids: &[String],
    ) -> Result<Vec<TextFileVersionReadResult>, BatchTextReadError> {
        validate_batch_text_ids(file_version_ids)?;
        let metadata = self
            .load_file_version_projection_metadata_batch(tenant, file_version_ids)
            .await
            .map_err(BatchTextReadError::Storage)?;
        let mut response_bytes = 0usize;
        let mut files = Vec::with_capacity(file_version_ids.len());
        for file_version_id in file_version_ids {
            let result = self
                .read_single_file_version_text(
                    tenant,
                    file_version_id.clone(),
                    metadata.get(file_version_id).cloned(),
                )
                .await?;
            consume_response_budget(file_version_id, result.text.len(), &mut response_bytes)?;
            files.push(result);
        }
        Ok(files)
    }

    async fn read_single_file_text(
        &self,
        tenant: &TenantId,
        file_id: String,
        meta: Option<FileProjectionMeta>,
    ) -> Result<TextFileReadResult, BatchTextReadError> {
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
            .await
            .map_err(BatchTextReadError::Storage)?
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
    ) -> Result<TextFileVersionReadResult, BatchTextReadError> {
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
            .await
            .map_err(BatchTextReadError::Storage)?
            .unwrap_or_default();
        Ok(TextFileVersionReadResult {
            file_version_id,
            found: true,
            content_hash: meta.content_hash,
            mime_type: meta.mime_type,
            text,
        })
    }
}

fn consume_response_budget(
    id: &str,
    item_bytes: usize,
    response_bytes: &mut usize,
) -> Result<(), BatchTextReadError> {
    if item_bytes > MAX_BATCH_ITEM_BYTES {
        return Err(BatchTextReadError::ItemTooLarge {
            id: id.to_string(),
            bytes: item_bytes,
        });
    }
    *response_bytes = response_bytes.saturating_add(item_bytes);
    if *response_bytes > MAX_BATCH_RESPONSE_BYTES {
        return Err(BatchTextReadError::ResponseTooLarge {
            bytes: *response_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_and_oversized_identifier_sets() {
        assert!(matches!(
            validate_batch_text_ids(&["same".to_string(), "same".to_string()]),
            Err(BatchTextReadError::InvalidRequest(_))
        ));
        assert!(matches!(
            validate_batch_text_ids(&vec!["id".to_string(); MAX_BATCH_ITEMS + 1]),
            Err(BatchTextReadError::TooManyItems { .. })
        ));
    }

    #[test]
    fn response_budget_rejects_large_items_and_aggregate_overflow() {
        let mut used = 0;
        assert!(matches!(
            consume_response_budget("large", MAX_BATCH_ITEM_BYTES + 1, &mut used),
            Err(BatchTextReadError::ItemTooLarge { .. })
        ));
        used = MAX_BATCH_RESPONSE_BYTES;
        assert!(matches!(
            consume_response_budget("next", 1, &mut used),
            Err(BatchTextReadError::ResponseTooLarge { .. })
        ));
    }
}
