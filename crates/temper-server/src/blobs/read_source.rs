//! Bounded reads from staged bytes, object storage, or the permitted legacy DB.
use crate::blob_store::{BlobReadBounded, BlobStore};
use crate::state::ServerState;
use temper_runtime::tenant::TenantId;

/// Available sources for bounded field-overflow reads.
pub(crate) enum BlobReadSource<'a> {
    #[cfg(test)]
    Store(&'a BlobStore),
    Staged {
        store: Option<&'a BlobStore>,
        legacy: Option<&'a dyn crate::storage::BlobStore>,
        blobs: &'a [super::OverflowBlobWrite],
    },
    Tenant {
        state: &'a ServerState,
        tenant: &'a TenantId,
    },
}

pub(super) async fn read_blob_ref_bytes(
    source: &BlobReadSource<'_>,
    key: &str,
    max_bytes: usize,
) -> Result<BlobReadBounded, String> {
    match source {
        #[cfg(test)]
        BlobReadSource::Store(store) => store.get_bounded(key, max_bytes).await,
        BlobReadSource::Staged {
            store,
            legacy,
            blobs,
        } => {
            if let Some(blob) = blobs.iter().find(|blob| blob.key == key) {
                return if blob.body.len() <= max_bytes {
                    Ok(BlobReadBounded::Found(blob.body.clone()))
                } else {
                    Ok(BlobReadBounded::TooLarge {
                        actual_bytes: Some(blob.body.len() as u64),
                    })
                };
            }
            if let Some(store) = store {
                match store.get_bounded(key, max_bytes).await {
                    Ok(found @ (BlobReadBounded::Found(_) | BlobReadBounded::TooLarge { .. })) => {
                        return Ok(found);
                    }
                    Ok(BlobReadBounded::Missing) => {}
                    Err(error) => {
                        tracing::warn!(%key, %error, "bounded object blob read failed; trying legacy DB fallback")
                    }
                }
            }
            match legacy {
                Some(legacy) => legacy
                    .get_blob_if_size_at_most(key, max_bytes)
                    .await
                    .map(|bytes| bytes.map_or(BlobReadBounded::Missing, BlobReadBounded::Found)),
                None => Ok(BlobReadBounded::Missing),
            }
        }
        BlobReadSource::Tenant { state, tenant } => {
            state
                .get_blob_with_legacy_fallback_bounded(tenant, key, max_bytes)
                .await
        }
    }
}
